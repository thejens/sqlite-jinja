//! sqlite-jinja: Jinja2 templates in SQLite, rendered by MiniJinja.
//!
//! Built with the default `loadable` feature, this crate is a run-time
//! loadable SQLite extension. Built with `--no-default-features --features
//! static`, it links SQLite statically and Rust programs can call
//! [`register`] on their own `rusqlite` connection.

mod environment;
mod functions;
mod inline;
mod sqlite;
mod stored;

/// Registers the jinja_* functions on a rusqlite connection.
#[cfg(feature = "static")]
pub fn register(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // SAFETY: the handle belongs to an open connection that outlives this
    // call; the registered functions are dropped by SQLite when it closes.
    unsafe { functions::register_raw(conn.handle()) }
        .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))
}

#[cfg(feature = "loadable")]
mod entry {
    use std::os::raw::{c_char, c_int};

    use libsqlite3_sys as ffi;

    use crate::{functions, sqlite};

    /// Entry point SQLite looks for when loading `libsqlite_jinja` without
    /// naming one: `sqlite3_` + the file name's letters, minus a `lib`
    /// prefix, + `_init`.
    ///
    /// # Safety
    /// Called by SQLite with valid pointers.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn sqlite3_sqlitejinja_init(
        db: *mut ffi::sqlite3,
        pz_err_msg: *mut *mut c_char,
        p_api: *mut ffi::sqlite3_api_routines,
    ) -> c_int {
        unsafe { init(db, pz_err_msg, p_api) }
    }

    /// Generic entry point, for hosts that load extensions under another
    /// file name.
    ///
    /// # Safety
    /// Called by SQLite with valid pointers.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn sqlite3_extension_init(
        db: *mut ffi::sqlite3,
        pz_err_msg: *mut *mut c_char,
        p_api: *mut ffi::sqlite3_api_routines,
    ) -> c_int {
        unsafe { init(db, pz_err_msg, p_api) }
    }

    unsafe fn init(
        db: *mut ffi::sqlite3,
        pz_err_msg: *mut *mut c_char,
        p_api: *mut ffi::sqlite3_api_routines,
    ) -> c_int {
        // SAFETY: SQLite's own pointers, passed straight through.
        unsafe {
            if !sqlite::init_api(p_api, pz_err_msg) {
                return ffi::SQLITE_ERROR;
            }
            match functions::register_raw(db) {
                // The functions belong to this connection only; every new
                // connection loads the extension again.
                Ok(()) => ffi::SQLITE_OK,
                Err(msg) => {
                    sqlite::set_error_message(pz_err_msg, &msg);
                    ffi::SQLITE_ERROR
                }
            }
        }
    }
}
