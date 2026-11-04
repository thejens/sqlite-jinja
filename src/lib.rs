//! sqlite-jinja: Jinja2 templates in SQLite, rendered by MiniJinja.
//!
//! Built with the default `loadable` feature, this crate is a run-time
//! loadable SQLite extension. Built with `--no-default-features --features
//! static`, it links SQLite statically and Rust programs can call
//! [`register`] on their own `rusqlite` connection.

mod environment;
mod functions;
mod inline;
mod stored;

pub use functions::register;

#[cfg(feature = "loadable")]
mod entry {
    use std::os::raw::{c_char, c_int};

    use rusqlite::{Connection, ffi};

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
        unsafe { Connection::extension_init2(db, pz_err_msg, p_api, init) }
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
        unsafe { Connection::extension_init2(db, pz_err_msg, p_api, init) }
    }

    fn init(db: Connection) -> rusqlite::Result<bool> {
        super::register(&db)?;
        // false: the functions belong to this connection only; every new
        // connection loads the extension again.
        Ok(false)
    }
}
