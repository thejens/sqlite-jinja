//! Stored templates: rows of the `jinja_templates` table, rendered by name.
//!
//! SQLite has no cross-connection cache-invalidation signal, so freshness is
//! checked against the table instead. Every cached template carries an
//! up-to-date check that re-reads its row and compares the source, and the
//! check runs at most once per SQL statement: the first call in a statement
//! verifies every template it touches, and the function records that in
//! SQLite's per-statement auxiliary data for the name argument. Later rows of
//! the same statement then render without touching the table.
//!
//! This gives each statement a consistent view of the templates, much like a
//! STABLE function in PostgreSQL. It also picks up changes from any source:
//! this connection, other connections and processes, and rolled-back
//! transactions. No triggers or bookkeeping tables are needed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libsqlite3_sys as ffi;
use minijinja::{Environment, Error, ErrorKind, Value};

use crate::environment::base_environment;

pub const TABLE_DDL: &str =
    "CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL)";

/// Raw handle of the connection the functions are registered on.
///
/// SAFETY: it is only dereferenced from inside this connection's own SQL
/// function calls, where SQLite guarantees the connection is open and
/// serializes access to it. The engines holding it are owned by those
/// functions and are dropped with them when the connection closes.
#[derive(Clone, Copy)]
pub struct DbHandle(pub *mut ffi::sqlite3);
unsafe impl Send for DbHandle {}
unsafe impl Sync for DbHandle {}

impl DbHandle {
    fn source_of(self, name: &str) -> Result<Option<String>, String> {
        // SAFETY: see the type's invariant.
        unsafe {
            crate::sqlite::query_text(
                self.0,
                c"SELECT source FROM jinja_templates WHERE name = ?1",
                name,
            )
        }
    }
}

pub struct StoredEngine {
    env: Environment<'static>,
    /// While set, up-to-date checks pass without querying, because the
    /// current statement has already verified its templates.
    trust_cache: Arc<AtomicBool>,
}

impl StoredEngine {
    pub fn new(strict: bool, db: DbHandle) -> Self {
        let trust_cache = Arc::new(AtomicBool::new(false));
        let mut env = base_environment(strict);
        // Consult each cached template's up-to-date check on every lookup,
        // including lookups for `extends`, `include` and `import`.
        env.set_auto_reload(true);
        let trust = trust_cache.clone();
        env.set_loader(move |name| {
            let Some(source) = db.source_of(name).map_err(|e| load_error(name, e))? else {
                return Ok(None);
            };
            let (trust, name, cached) = (trust.clone(), name.to_owned(), source.clone());
            Ok(Some(
                minijinja::TemplateSource::new(source).with_uptodate_check(move || {
                    // Any error reads as stale, so the loader runs again and
                    // reports it.
                    trust.load(Ordering::Relaxed)
                        || matches!(db.source_of(&name), Ok(Some(s)) if s == cached)
                }),
            ))
        });
        Self { env, trust_cache }
    }

    /// Renders `name`. `verified_in_statement` says whether an earlier call
    /// in the same SQL statement already checked the cached templates.
    pub fn render(
        &mut self,
        name: &str,
        ctx: Value,
        fuel: Option<u64>,
        verified_in_statement: bool,
    ) -> Result<String, Error> {
        self.trust_cache
            .store(verified_in_statement, Ordering::Relaxed);
        self.env.set_fuel(fuel);
        let result = self.env.get_template(name).and_then(|t| t.render(ctx));
        self.trust_cache.store(false, Ordering::Relaxed);
        result
    }
}

fn load_error(name: &str, err: String) -> Error {
    let hint = if err.contains("no such table") {
        format!(" (create it with: {TABLE_DDL})")
    } else {
        String::new()
    };
    Error::new(
        ErrorKind::InvalidOperation,
        format!("could not load template {name:?}: {err}{hint}"),
    )
}
