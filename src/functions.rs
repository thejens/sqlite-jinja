//! The SQL functions and their registration on a connection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Error, Result};

use crate::environment::validate;
use crate::inline::InlineEngine;
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

/// Marker stored as SQLite auxiliary data on `jinja_render_named`'s name
/// argument once the statement has verified its cached templates.
struct VerifiedInStatement;

/// Registers the jinja_* functions on `conn`.
pub fn register(conn: &Connection) -> Result<()> {
    // SAFETY: see `DbHandle`; the handle is only used from inside calls to
    // the functions registered below.
    let db = DbHandle(unsafe { conn.handle() });
    let state = Arc::new(Jinja {
        inline: [
            Mutex::new(InlineEngine::new(false)),
            Mutex::new(InlineEngine::new(true)),
        ],
        stored: [
            Mutex::new(StoredEngine::new(false, db)),
            Mutex::new(StoredEngine::new(true, db)),
        ],
        fuel: AtomicU64::new(DEFAULT_FUEL),
    });

    // DETERMINISTIC lets SQLite use these in generated columns, indexes and
    // CHECK constraints. INNOCUOUS allows them in views and triggers even
    // with trusted_schema=OFF: they read nothing and change nothing.
    let pure = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;

    for n_arg in 1..=4 {
        let state = state.clone();
        conn.create_scalar_function(c"jinja_render", n_arg, pure, move |ctx| {
            render_inline(&state, ctx)
        })?;
    }

    // Reads jinja_templates, so its result can change between statements:
    // neither DETERMINISTIC nor INNOCUOUS.
    for n_arg in 1..=3 {
        let state = state.clone();
        conn.create_scalar_function(
            c"jinja_render_named",
            n_arg,
            FunctionFlags::SQLITE_UTF8,
            move |ctx| render_named(&state, ctx),
        )?;
    }

    conn.create_scalar_function(c"jinja_is_valid", 1, pure, |ctx| {
        Ok(match ctx.get_raw(0) {
            ValueRef::Null => None,
            _ => Some(validate(text_arg(ctx, 0, "template")?).is_ok()),
        })
    })?;

    conn.create_scalar_function(c"jinja_syntax_error", 1, pure, |ctx| {
        Ok(match ctx.get_raw(0) {
            ValueRef::Null => None,
            _ => validate(text_arg(ctx, 0, "template")?)
                .err()
                .map(|e| e.to_string()),
        })
    })?;

    // Changes connection state: DIRECTONLY keeps it out of views, triggers
    // and schema objects, where a database file could call it behind the
    // user's back.
    let fuel_state = state.clone();
    conn.create_scalar_function(
        c"jinja_set_fuel",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY,
        move |ctx| {
            let fuel: i64 = ctx.get(0)?;
            let fuel = u64::try_from(fuel)
                .map_err(|_| user_error("jinja_set_fuel: fuel must be >= 0 (0 = unlimited)"))?;
            Ok(fuel_state.fuel.swap(fuel, Ordering::Relaxed) as i64)
        },
    )?;

    Ok(())
}

fn render_inline(state: &Jinja, ctx: &Context<'_>) -> Result<Option<String>> {
    if matches!(ctx.get_raw(0), ValueRef::Null) {
        return Ok(None);
    }
    let source = text_arg(ctx, 0, "template")?;
    let context = context_arg(ctx, 1, "jinja_render")?;
    let strict = flag_arg(ctx, 2)?;
    let autoescape = flag_arg(ctx, 3)?;
    let mut engine = lock(&state.inline[strict as usize])?;
    engine
        .render(source, autoescape, context, state.fuel())
        .map(Some)
        .map_err(|e| render_error("template render failed", &e))
}

fn render_named(state: &Jinja, ctx: &Context<'_>) -> Result<Option<String>> {
    if matches!(ctx.get_raw(0), ValueRef::Null) {
        return Ok(None);
    }
    let name = text_arg(ctx, 0, "name")?;
    let context = context_arg(ctx, 1, "jinja_render_named")?;
    let strict = flag_arg(ctx, 2)?;
    let verified = ctx.get_aux::<VerifiedInStatement>(0)?.is_some();
    let mut engine = lock(&state.stored[strict as usize])?;
    let out = engine
        .render(name, context, state.fuel(), verified)
        .map_err(|e| render_error(&format!("rendering template {name:?} failed"), &e))?;
    if !verified {
        // SQLite keeps this for the rest of the statement as long as the
        // argument stays the same, and discards it as soon as it changes,
        // so a per-row template name simply gets checked on every row.
        ctx.set_aux(0, VerifiedInStatement)?;
    }
    Ok(Some(out))
}

/// Locks an engine, refusing re-entrant use instead of deadlocking. That
/// happens if rendering a template runs SQL that renders again on the same
/// connection, for example when `jinja_templates` is a view that calls
/// `jinja_render_named`.
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(guard) => Ok(guard),
        // A panic mid-render leaves only a cache behind, which is safe to
        // keep using.
        Err(TryLockError::Poisoned(p)) => Ok(p.into_inner()),
        Err(TryLockError::WouldBlock) => Err(user_error(
            "jinja: recursive rendering on the same connection is not supported",
        )),
    }
}

fn text_arg<'a>(ctx: &'a Context<'_>, idx: usize, what: &str) -> Result<&'a str> {
    match ctx.get_raw(idx) {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| user_error(&format!("{what} is not valid UTF-8"))),
        other => Err(user_error(&format!(
            "{what} must be text, got {}",
            sql_type(other)
        ))),
    }
}

/// Parses the JSON context argument. Missing or NULL means empty.
fn context_arg(ctx: &Context<'_>, idx: usize, function: &str) -> Result<minijinja::Value> {
    if idx >= ctx.len() {
        return Ok(minijinja::Value::from(()));
    }
    let json = match ctx.get_raw(idx) {
        ValueRef::Null => return Ok(minijinja::Value::from(())),
        ValueRef::Text(bytes) => bytes,
        ValueRef::Blob(_) => {
            return Err(user_error(&format!(
                "{function} context must be JSON text; wrap JSONB blobs in json()"
            )));
        }
        other => {
            return Err(user_error(&format!(
                "{function} context must be a JSON object, got {}",
                sql_type(other)
            )));
        }
    };
    match serde_json::from_slice::<serde_json::Value>(json) {
        Ok(v @ serde_json::Value::Object(_)) => {
            Ok(minijinja::Value::from(minijinja::value::Serde(v)))
        }
        Ok(serde_json::Value::Null) => Ok(minijinja::Value::from(())),
        Ok(other) => Err(user_error(&format!(
            "{function} context must be a JSON object, got {}",
            json_type(&other)
        ))),
        Err(e) => Err(user_error(&format!(
            "{function} context is not valid JSON: {e}"
        ))),
    }
}

/// Optional boolean argument; missing or NULL is false.
fn flag_arg(ctx: &Context<'_>, idx: usize) -> Result<bool> {
    if idx >= ctx.len() {
        return Ok(false);
    }
    Ok(ctx.get::<Option<bool>>(idx)?.unwrap_or(false))
}

fn render_error(message: &str, err: &minijinja::Error) -> Error {
    user_error(&format!("{message}: {err}"))
}

fn user_error(message: &str) -> Error {
    Error::UserFunctionError(message.into())
}

/// SQLite's own lower-case name for a value's storage class.
fn sql_type(v: ValueRef<'_>) -> &'static str {
    match v {
        ValueRef::Null => "null",
        ValueRef::Integer(_) => "integer",
        ValueRef::Real(_) => "real",
        ValueRef::Text(_) => "text",
        ValueRef::Blob(_) => "blob",
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
