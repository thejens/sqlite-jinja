//! A minimal, safe wrapper over the parts of the SQLite C API this extension
//! uses: registering scalar functions, reading their arguments, setting
//! results, per-statement auxiliary data, and one-row lookup queries.
//!
//! The extension talks to SQLite through `libsqlite3-sys` directly rather
//! than through rusqlite. rusqlite calls SQLite 3.38–3.44 APIs
//! unconditionally and refuses to start on anything older. This layer only
//! uses APIs from 3.30 or earlier, so the extension's minimum SQLite version
//! is set by the headers it is built against (see sqlite-headers/).

use std::ffi::{CStr, c_int, c_void};
#[cfg(feature = "loadable")]
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use libsqlite3_sys as ffi;

/// One SQL argument value. Borrowed text is valid for the duration of the
/// function call it was read in. Blob contents are never needed, only the
/// fact that an argument is one.
#[derive(Clone, Copy)]
pub enum Arg<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob,
}

impl Arg<'_> {
    /// SQLite's lower-case name for the value's storage class.
    pub fn type_name(self) -> &'static str {
        match self {
            Arg::Null => "null",
            Arg::Integer(_) => "integer",
            Arg::Real(_) => "real",
            Arg::Text(_) => "text",
            Arg::Blob => "blob",
        }
    }
}

/// The value a scalar function returns.
pub enum Output {
    Null,
    Integer(i64),
    Text(String),
}

/// Argument access for one call of a scalar function.
pub struct Call<'a> {
    ctx: *mut ffi::sqlite3_context,
    args: &'a [*mut ffi::sqlite3_value],
}

impl<'a> Call<'a> {
    /// Argument `i`, or `Arg::Null` when the call has fewer arguments.
    pub fn arg(&self, i: usize) -> Arg<'a> {
        let Some(&v) = self.args.get(i) else {
            return Arg::Null;
        };
        // SAFETY: `v` is a live argument of the current call. For text and
        // blobs, the pointer must be read *before* the length:
        // sqlite3_value_bytes may convert the value, invalidating an earlier
        // pointer, but not one fetched first.
        unsafe {
            match ffi::sqlite3_value_type(v) {
                ffi::SQLITE_INTEGER => Arg::Integer(ffi::sqlite3_value_int64(v)),
                ffi::SQLITE_FLOAT => Arg::Real(ffi::sqlite3_value_double(v)),
                ffi::SQLITE_TEXT => {
                    let p = ffi::sqlite3_value_text(v);
                    let n = ffi::sqlite3_value_bytes(v);
                    Arg::Text(slice(p.cast(), n))
                }
                ffi::SQLITE_BLOB => Arg::Blob,
                _ => Arg::Null,
            }
        }
    }

    /// Whether this statement already attached the marker to argument `i`
    /// via [`Call::mark_statement`].
    pub fn is_marked(&self, i: usize) -> bool {
        // SAFETY: valid context; `i` is a valid argument index (an argument the
        // function was registered with).
        !unsafe { ffi::sqlite3_get_auxdata(self.ctx, i as c_int) }.is_null()
    }

    /// Attaches a marker to argument `i` for the rest of the statement.
    /// SQLite keeps it while the argument value stays the same and drops it
    /// as soon as the value changes or the statement ends.
    pub fn mark_statement(&self, i: usize) {
        static MARKER: u8 = 0;
        // SAFETY: the pointer refers to a static, so no destructor is needed.
        unsafe {
            ffi::sqlite3_set_auxdata(
                self.ctx,
                i as c_int,
                ptr::addr_of!(MARKER).cast_mut().cast(),
                None,
            )
        };
    }
}

/// A scalar function's implementation. An `Err` becomes a SQL error with
/// that message.
pub type ScalarFn = Box<dyn Fn(&Call<'_>) -> Result<Output, String>>;

/// Registers a scalar function on `db`. The closure is freed by SQLite when
/// the function is replaced or the connection closes.
///
/// # Safety
/// `db` must be an open connection.
pub unsafe fn create_function(
    db: *mut ffi::sqlite3,
    name: &CStr,
    n_arg: c_int,
    flags: c_int,
    f: ScalarFn,
) -> Result<(), String> {
    let app = Box::into_raw(Box::new(f));
    // SAFETY: `app` is a valid boxed closure whose ownership passes to
    // SQLite, which frees it with `destroy`, including when registration
    // fails.
    let rc = unsafe {
        ffi::sqlite3_create_function_v2(
            db,
            name.as_ptr(),
            n_arg,
            flags,
            app.cast(),
            Some(trampoline),
            None,
            None,
            Some(destroy),
        )
    };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(unsafe { errmsg(db) })
    }
}

unsafe extern "C" fn trampoline(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite passes the pointer registered in `create_function` and
    // an argument array of `argc` live values.
    let (f, args) = unsafe {
        let f = &*ffi::sqlite3_user_data(ctx).cast::<ScalarFn>();
        let args: &[*mut ffi::sqlite3_value] = if argc == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(argv, argc as usize)
        };
        (f, args)
    };
    let call = Call { ctx, args };
    // A panic must not unwind into SQLite's C frames.
    let result = catch_unwind(AssertUnwindSafe(|| f(&call)))
        .unwrap_or_else(|_| Err("internal error in sqlite-jinja (panic)".to_owned()));
    // SAFETY: valid context; text results are copied by SQLite (TRANSIENT).
    unsafe {
        match result {
            Ok(Output::Null) => ffi::sqlite3_result_null(ctx),
            Ok(Output::Integer(n)) => ffi::sqlite3_result_int64(ctx, n),
            Ok(Output::Text(s)) => ffi::sqlite3_result_text64(
                ctx,
                s.as_ptr().cast(),
                s.len() as ffi::sqlite3_uint64,
                ffi::SQLITE_TRANSIENT(),
                ffi::SQLITE_UTF8 as u8,
            ),
            Err(msg) => ffi::sqlite3_result_error(ctx, msg.as_ptr().cast(), len_c_int(msg.len())),
        }
    }
}

unsafe extern "C" fn destroy(app: *mut c_void) {
    // SAFETY: `app` came from `Box::into_raw` in `create_function`.
    drop(unsafe { Box::from_raw(app.cast::<ScalarFn>()) });
}

/// Runs a query with one text parameter and returns the first column of the
/// first row as text, or `None` if there are no rows.
///
/// # Safety
/// `db` must be an open connection, used from the thread that holds it
/// (in practice: from inside one of its own function calls).
pub unsafe fn query_text(
    db: *mut ffi::sqlite3,
    sql: &CStr,
    param: &str,
) -> Result<Option<String>, String> {
    let mut stmt = ptr::null_mut();
    // SAFETY: valid connection; the statement is finalized on every path
    // below, so nothing prepared here outlives the call.
    unsafe {
        if ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
            != ffi::SQLITE_OK
        {
            return Err(errmsg(db));
        }
        let result = (|| {
            if ffi::sqlite3_bind_text(
                stmt,
                1,
                param.as_ptr().cast(),
                len_c_int(param.len()),
                ffi::SQLITE_TRANSIENT(),
            ) != ffi::SQLITE_OK
            {
                return Err(errmsg(db));
            }
            match ffi::sqlite3_step(stmt) {
                ffi::SQLITE_ROW => {
                    let p = ffi::sqlite3_column_text(stmt, 0);
                    let n = ffi::sqlite3_column_bytes(stmt, 0);
                    if p.is_null() {
                        return Ok(None);
                    }
                    String::from_utf8(slice(p.cast(), n).to_vec())
                        .map(Some)
                        .map_err(|_| "query returned text that is not valid UTF-8".to_owned())
                }
                ffi::SQLITE_DONE => Ok(None),
                _ => Err(errmsg(db)),
            }
        })();
        ffi::sqlite3_finalize(stmt);
        result
    }
}

/// Initialises the API routine table handed to a loadable extension. Must
/// run before any other SQLite call in a loadable build.
///
/// On failure the message is already stored in `pz_err_msg`. It is written
/// through the routine table's own `mprintf`, because the wrapper functions
/// are not usable until initialisation succeeds.
///
/// # Safety
/// `p_api` and `pz_err_msg` must be the pointers SQLite passed to the entry
/// point.
#[cfg(feature = "loadable")]
pub unsafe fn init_api(
    p_api: *mut ffi::sqlite3_api_routines,
    pz_err_msg: *mut *mut c_char,
) -> bool {
    if p_api.is_null() {
        return false;
    }
    // SAFETY: `p_api` is SQLite's routine table.
    let Err(err) = (unsafe { ffi::rusqlite_extension_init2(p_api) }) else {
        return true;
    };
    let msg = match err {
        ffi::InitError::VersionMismatch {
            compile_time,
            runtime,
        } => format!(
            "sqlite-jinja requires SQLite {} or newer, but this is SQLite {}",
            version_string(compile_time),
            version_string(runtime)
        ),
        other => format!("sqlite-jinja could not initialise the SQLite API: {other:?}"),
    };
    // SAFETY: `mprintf` comes straight from the routine table; "%s" with a
    // valid NUL-terminated string.
    unsafe {
        if let (false, Some(mprintf)) = (pz_err_msg.is_null(), (*p_api).mprintf) {
            let msg = CString::new(msg).unwrap_or_default();
            *pz_err_msg = mprintf(c"%s".as_ptr(), msg.as_ptr());
        }
    }
    false
}

#[cfg(feature = "loadable")]
fn version_string(n: c_int) -> String {
    format!("{}.{}.{}", n / 1_000_000, n / 1000 % 1000, n % 1000)
}

/// Copies `msg` into SQLite-allocated memory for an entry point's
/// `pzErrMsg`, which SQLite frees.
///
/// # Safety
/// Only after the API table is initialised; `pz_err_msg` may be null.
#[cfg(feature = "loadable")]
pub unsafe fn set_error_message(pz_err_msg: *mut *mut c_char, msg: &str) {
    if pz_err_msg.is_null() {
        return;
    }
    // sqlite3_mprintf is variadic and has no generated wrapper, so copy the
    // bytes into sqlite3_malloc64 memory, which is what SQLite frees.
    let bytes = msg.replace('\0', " ");
    // SAFETY: allocating len + 1 bytes and writing exactly that many.
    unsafe {
        let buf = ffi::sqlite3_malloc64(bytes.len() as ffi::sqlite3_uint64 + 1).cast::<u8>();
        if buf.is_null() {
            return;
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
        *buf.add(bytes.len()) = 0;
        *pz_err_msg = buf.cast();
    }
}

unsafe fn errmsg(db: *mut ffi::sqlite3) -> String {
    // SAFETY: sqlite3_errmsg always returns a valid string for an open db.
    unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned()
}

/// Builds a slice from a pointer and a length SQLite returned; a null
/// pointer (an empty value) becomes an empty slice.
unsafe fn slice<'a>(p: *const u8, n: c_int) -> &'a [u8] {
    if p.is_null() || n <= 0 {
        &[]
    } else {
        // SAFETY: SQLite guarantees `n` readable bytes at `p`.
        unsafe { std::slice::from_raw_parts(p, n as usize) }
    }
}

/// Lengths passed to SQLite are C ints; anything larger is truncated, which
/// for error messages is harmless.
fn len_c_int(n: usize) -> c_int {
    c_int::try_from(n).unwrap_or(c_int::MAX)
}
