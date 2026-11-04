//! The SQL functions and their registration on a connection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use libsqlite3_sys as ffi;

use crate::environment::validate;
use crate::inline::InlineEngine;
use crate::sqlite::{Arg, Call, Output, create_function};
use crate::stored::{DbHandle, StoredEngine};

/// Default instruction budget for one render. It guards against templates
/// such as `{% for i in range(10**9) %}`: a render never returns control to
/// SQLite until it finishes, so `sqlite3_interrupt` cannot stop it.
const DEFAULT_FUEL: u64 = 10_000_000;

/// Per-connection state, shared by all of the connection's functions.
struct Jinja {
    /// Indexed by `strict as usize`.
    inline: [Mutex<InlineEngine>; 2],
    stored: [Mutex<StoredEngine>; 2],
    /// 0 means unlimited.
    fuel: AtomicU64,
}

impl Jinja {
    fn fuel(&self) -> Option<u64> {
        match self.fuel.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }
}

/// Registers the jinja_* functions on the connection `db`.
///
/// # Safety
/// `db` must be an open connection. In a loadable build the SQLite API table
/// must already be initialised.
pub unsafe fn register_raw(db: *mut ffi::sqlite3) -> Result<(), String> {
    let handle = DbHandle(db);
    let state = Arc::new(Jinja {
        inline: [
            Mutex::new(InlineEngine::new(false)),
            Mutex::new(InlineEngine::new(true)),
        ],
        stored: [
            Mutex::new(StoredEngine::new(false, handle)),
            Mutex::new(StoredEngine::new(true, handle)),
        ],
        fuel: AtomicU64::new(DEFAULT_FUEL),
    });

    // DETERMINISTIC lets SQLite use these in generated columns, indexes and
    // CHECK constraints. INNOCUOUS allows them in views and triggers even
    // with trusted_schema=OFF: they read nothing and change nothing.
    let pure = ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC | ffi::SQLITE_INNOCUOUS;
    // Reads jinja_templates, so its result can change between statements:
    // neither DETERMINISTIC nor INNOCUOUS.
    let reads_table = ffi::SQLITE_UTF8;
    // Changes connection state: DIRECTONLY keeps it out of views, triggers
    // and schema objects, where a database file could call it behind the
    // user's back.
    let stateful = ffi::SQLITE_UTF8 | ffi::SQLITE_DIRECTONLY;

    // SAFETY (all calls below): `db` is open, per this function's contract.
    for n_arg in 1..=4 {
        let state = state.clone();
        unsafe {
            create_function(
                db,
                c"jinja_render",
                n_arg,
                pure,
                Box::new(move |call| render_inline(&state, call)),
            )?
        };
    }
    for n_arg in 1..=3 {
        let state = state.clone();
        unsafe {
            create_function(
                db,
                c"jinja_render_named",
                n_arg,
                reads_table,
                Box::new(move |call| render_named(&state, call)),
            )?
        };
    }
    unsafe {
        create_function(
            db,
            c"jinja_is_valid",
            1,
            pure,
            Box::new(|call| {
                Ok(match call.arg(0) {
                    Arg::Null => Output::Null,
                    _ => Output::Integer(validate(text_arg(call, 0, "template")?).is_ok() as i64),
                })
            }),
        )?;
        create_function(
            db,
            c"jinja_syntax_error",
            1,
            pure,
            Box::new(|call| {
                Ok(match call.arg(0) {
                    Arg::Null => Output::Null,
                    _ => match validate(text_arg(call, 0, "template")?) {
                        Ok(()) => Output::Null,
                        Err(e) => Output::Text(e.to_string()),
                    },
                })
            }),
        )?;
        let fuel_state = state.clone();
        create_function(
            db,
            c"jinja_set_fuel",
            1,
            stateful,
            Box::new(move |call| {
                let Arg::Integer(fuel) = call.arg(0) else {
                    return Err("jinja_set_fuel: fuel must be an integer (0 = unlimited)".into());
                };
                let fuel = u64::try_from(fuel)
                    .map_err(|_| "jinja_set_fuel: fuel must be >= 0 (0 = unlimited)".to_owned())?;
                Ok(Output::Integer(
                    fuel_state.fuel.swap(fuel, Ordering::Relaxed) as i64,
                ))
            }),
        )?;
    }
    Ok(())
}

fn render_inline(state: &Jinja, call: &Call<'_>) -> Result<Output, String> {
    if matches!(call.arg(0), Arg::Null) {
        return Ok(Output::Null);
    }
    let source = text_arg(call, 0, "template")?;
    let context = context_arg(call, 1, "jinja_render")?;
    let strict = flag_arg(call, 2, "strict")?;
    let autoescape = flag_arg(call, 3, "autoescape")?;
    let mut engine = lock(&state.inline[strict as usize])?;
    engine
        .render(source, autoescape, context, state.fuel())
        .map(Output::Text)
        .map_err(|e| format!("template render failed: {e}"))
}

fn render_named(state: &Jinja, call: &Call<'_>) -> Result<Output, String> {
    if matches!(call.arg(0), Arg::Null) {
        return Ok(Output::Null);
    }
    let name = text_arg(call, 0, "name")?;
    let context = context_arg(call, 1, "jinja_render_named")?;
    let strict = flag_arg(call, 2, "strict")?;
    let verified = call.is_marked(0);
    let mut engine = lock(&state.stored[strict as usize])?;
    let out = engine
        .render(name, context, state.fuel(), verified)
        .map_err(|e| format!("rendering template {name:?} failed: {e}"))?;
    if !verified {
        // SQLite keeps the marker for the rest of the statement as long as
        // the argument stays the same, and drops it as soon as it changes,
        // so a per-row template name simply gets checked on every row.
        call.mark_statement(0);
    }
    Ok(Output::Text(out))
}

/// Locks an engine, refusing re-entrant use instead of deadlocking. That
/// happens if rendering a template runs SQL that renders again on the same
/// connection, for example when `jinja_templates` is a view that calls
/// `jinja_render_named`.
fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, String> {
    match m.try_lock() {
        Ok(guard) => Ok(guard),
        // A panic mid-render leaves only a cache behind, which is safe to
        // keep using.
        Err(TryLockError::Poisoned(p)) => Ok(p.into_inner()),
        Err(TryLockError::WouldBlock) => {
            Err("jinja: recursive rendering on the same connection is not supported".into())
        }
    }
}

fn text_arg<'a>(call: &Call<'a>, idx: usize, what: &str) -> Result<&'a str, String> {
    match call.arg(idx) {
        Arg::Text(bytes) => {
            std::str::from_utf8(bytes).map_err(|_| format!("{what} is not valid UTF-8"))
        }
        other => Err(format!("{what} must be text, got {}", other.type_name())),
    }
}

/// Parses the JSON context argument. Missing or NULL means empty.
fn context_arg(call: &Call<'_>, idx: usize, function: &str) -> Result<minijinja::Value, String> {
    let json = match call.arg(idx) {
        Arg::Null => return Ok(minijinja::Value::from(())),
        Arg::Text(bytes) => bytes,
        Arg::Blob => {
            return Err(format!(
                "{function} context must be JSON text; wrap JSONB blobs in json()"
            ));
        }
        other => {
            return Err(format!(
                "{function} context must be a JSON object, got {}",
                other.type_name()
            ));
        }
    };
    match serde_json::from_slice::<serde_json::Value>(json) {
        Ok(v @ serde_json::Value::Object(_)) => {
            Ok(minijinja::Value::from(minijinja::value::Serde(v)))
        }
        Ok(serde_json::Value::Null) => Ok(minijinja::Value::from(())),
        Ok(other) => Err(format!(
            "{function} context must be a JSON object, got {}",
            json_type(&other)
        )),
        Err(e) => Err(format!("{function} context is not valid JSON: {e}")),
    }
}

/// Optional boolean argument following SQLite's conventions: missing or NULL
/// is false, numbers are true when non-zero.
fn flag_arg(call: &Call<'_>, idx: usize, what: &str) -> Result<bool, String> {
    match call.arg(idx) {
        Arg::Null => Ok(false),
        Arg::Integer(n) => Ok(n != 0),
        Arg::Real(f) => Ok(f != 0.0),
        other => Err(format!("{what} must be 0 or 1, got {}", other.type_name())),
    }
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
