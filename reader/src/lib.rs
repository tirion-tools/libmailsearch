//! libmailsearch: the C ABI of the search reader,
//! exactly `include/mailsearch.h`. The engine is [`engine::Engine`] (its Rust API is documented
//! there).
//!
//! No panic crosses the boundary: the library is built with `--profile ffi` (panic = "unwind") and
//! every `mailsearch_*` entry point runs inside `catch_unwind`. A panic returns `MAILSEARCH_E_PANIC` (message:
//! `mailsearch_last_error`) and poisons the handle: every later call on it returns `MAILSEARCH_E_POISONED` until
//! `mailsearch_close`. `mailsearch_cancel` is the one call made from another thread: it only stores into the
//! handle's atomic cancel flag. A SIGBUS (a mapped file truncated under the reader) is not a panic
//! and cannot be caught here; the directory layout and the reader lease prevent it.

pub mod dir;
pub mod engine;
mod highlight;

use engine::{Clause, Engine, KeySet, Mode, OpenError, Opts, Query, SearchError};
use std::cell::{RefCell, UnsafeCell};
use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAILSEARCH_OK: i32 = 0;
pub const MAILSEARCH_E_ARG: i32 = -1;
pub const MAILSEARCH_E_IO: i32 = -2;
pub const MAILSEARCH_E_VERSION: i32 = -3;
pub const MAILSEARCH_E_PANIC: i32 = -4;
pub const MAILSEARCH_E_POISONED: i32 = -5;
pub const MAILSEARCH_E_CANCELLED: i32 = -6;
pub const MAILSEARCH_E_CORRUPT: i32 = -7;

pub const MAILSEARCH_C_PARTIAL: u32 = 1 << 0;
pub const MAILSEARCH_C_NEGATE: u32 = 1 << 1;

pub const MAILSEARCH_Q_TYPING: u32 = 1 << 0;
pub const MAILSEARCH_Q_SKIP_TOP: u32 = 1 << 1;
pub const MAILSEARCH_Q_SKIP_LIST: u32 = 1 << 2;
pub const MAILSEARCH_Q_RELEVANCE: u32 = 1 << 3;
pub const MAILSEARCH_Q_NO_COLLAPSE: u32 = 1 << 4;
pub const MAILSEARCH_Q_NO_EXCLUDE: u32 = 1 << 5;
const MAILSEARCH_Q_ALL: u32 = (1 << 6) - 1;

pub const MAILSEARCH_R_FROM_RECENT: u32 = 1 << 0;
pub const MAILSEARCH_R_CORRECTED: u32 = 1 << 1;

/// Top hits a query may ask for (`MailsearchQuery.n_top` 0..7).
pub const MAILSEARCH_MAX_TOP: u32 = 7;

#[repr(C)]
pub struct MailsearchClause {
    pub text: *const c_char,
    pub len: usize,
    pub fields: u32,
    pub mode: u32,
    pub flags: u32,
    pub group: u32,
}

#[repr(C)]
pub struct MailsearchQuery {
    pub clauses: *const MailsearchClause,
    pub n_clauses: usize,
    pub now: i64,
    pub date_lo: i64,
    pub date_hi: i64,
    pub hint_lo: i64,
    pub hint_hi: i64,
    pub before_key: i64,
    pub account: u64,
    pub flags: u32,
    pub n_top: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MailsearchHit {
    pub mid: u64,
    pub search_key: i64,
    pub score: f32,
    pub tiers: u32,
    pub fields: u32,
    pub copies: u32,
}

#[repr(C)]
pub struct MailsearchResult {
    pub top: *mut MailsearchHit,
    pub top_cap: usize,
    pub n_top: usize,
    pub list: *mut MailsearchHit,
    pub list_cap: usize,
    pub n_list: usize,
    pub count: u64,
    pub count_capped: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailsearchSpan {
    pub start: u32,
    pub end: u32,
    pub tier: u32,
}

struct Inner {
    e: Engine,
    o: Opts,
}

/// The opaque handle. The worker thread owns `inner`; `mailsearch_cancel` touches only `cancel`.
pub struct MailsearchHandle {
    cancel: Arc<AtomicBool>,
    poisoned: AtomicBool,
    inner: UnsafeCell<Inner>,
}

thread_local! {
    static LAST: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_last(s: impl Into<String>) {
    LAST.with(|l| *l.borrow_mut() = s.into());
}

fn panic_msg(p: &(dyn std::any::Any + Send)) -> String {
    let m = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown".into());
    format!("panic: {m}")
}

/// Returns `MAILSEARCH_E_ARG` with a message.
fn arg(msg: &str) -> i32 {
    set_last(msg);
    MAILSEARCH_E_ARG
}

/// Runs `f` on a live handle: `MAILSEARCH_E_ARG` for NULL, `MAILSEARCH_E_POISONED` after an earlier panic,
/// `MAILSEARCH_E_PANIC` (and poisoning) if `f` panics.
fn with(g: *mut MailsearchHandle, f: impl FnOnce(&mut Inner) -> i32) -> i32 {
    // SAFETY: a non-null `g` is a handle from mailsearch_open not yet closed (caller contract); only the
    // owning worker thread calls this, so the exclusive borrow of `inner` is unique.
    let Some(h) = (unsafe { g.as_ref() }) else {
        return arg("null handle");
    };
    if h.poisoned.load(Ordering::Acquire) {
        set_last("handle poisoned by an earlier panic");
        return MAILSEARCH_E_POISONED;
    }
    // SAFETY: see above; `cancel`, the only field other threads touch, is outside the cell.
    let inner = unsafe { &mut *h.inner.get() };
    match catch_unwind(AssertUnwindSafe(|| f(inner))) {
        Ok(r) => r,
        Err(p) => {
            h.poisoned.store(true, Ordering::Release);
            set_last(panic_msg(&*p));
            MAILSEARCH_E_PANIC
        }
    }
}

/// Catches a panic in a handle-less call.
fn guard(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            set_last(panic_msg(&*p));
            MAILSEARCH_E_PANIC
        }
    }
}

/// A UTF-8 argument with an explicit length (`None`: null with len > 0, or invalid UTF-8).
///
/// # Safety
/// `p` must be null or valid for `len` bytes for `'a`.
unsafe fn text<'a>(p: *const c_char, len: usize) -> Option<&'a str> {
    if len == 0 {
        return Some("");
    }
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    let b = unsafe { std::slice::from_raw_parts(p as *const u8, len) };
    std::str::from_utf8(b).ok()
}

/// A caller array (`None`: null with n > 0).
///
/// # Safety
/// `p` must be null or valid for `n` elements for `'a`.
unsafe fn slice<'a, T>(p: *const T, n: usize) -> Option<&'a [T]> {
    if n == 0 {
        return Some(&[]);
    }
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    Some(unsafe { std::slice::from_raw_parts(p, n) })
}

fn range(lo: i64, hi: i64) -> Result<Option<(i64, i64)>, i32> {
    if lo == 0 && hi == 0 {
        Ok(None)
    } else if hi < lo || lo < 0 {
        Err(arg("bad date range"))
    } else {
        Ok(Some((lo, hi)))
    }
}

/// The engine query of a `MailsearchQuery`; `Err` is the error code (message set).
///
/// # Safety
/// `q` must be a valid `MailsearchQuery` whose clause array and texts are valid for the call.
unsafe fn to_query(q: &MailsearchQuery, n_list: usize) -> Result<Query, i32> {
    if q.flags & !MAILSEARCH_Q_ALL != 0 {
        return Err(arg("unknown MAILSEARCH_Q_* flag"));
    }
    if q.n_top > MAILSEARCH_MAX_TOP {
        return Err(arg("n_top > 7"));
    }
    // SAFETY: caller contract.
    let cls = unsafe { slice(q.clauses, q.n_clauses) }.ok_or_else(|| arg("null clauses"))?;
    let mut clauses = Vec::with_capacity(cls.len());
    for c in cls {
        if c.fields == 0 || c.fields & !engine::F_TEXT != 0 {
            return Err(arg("bad clause fields"));
        }
        if c.flags & !(MAILSEARCH_C_PARTIAL | MAILSEARCH_C_NEGATE) != 0 {
            return Err(arg("unknown MAILSEARCH_C_* flag"));
        }
        let mode = match c.mode {
            0 => Mode::Default,
            1 => Mode::Exact,
            2 => Mode::Phrase,
            3 => Mode::Prefix,
            _ => return Err(arg("bad clause mode")),
        };
        // SAFETY: caller contract.
        let t =
            unsafe { text(c.text, c.len) }.ok_or_else(|| arg("clause text: null or not UTF-8"))?;
        clauses.push(Clause {
            text: t.to_string(),
            fields: c.fields,
            mode,
            partial: c.flags & MAILSEARCH_C_PARTIAL != 0,
            negate: c.flags & MAILSEARCH_C_NEGATE != 0,
            group: c.group,
        });
    }
    Ok(Query {
        clauses,
        now: q.now,
        date: range(q.date_lo, q.date_hi)?,
        hint: range(q.hint_lo, q.hint_hi)?,
        before_key: q.before_key.max(0),
        account: q.account,
        typing: q.flags & MAILSEARCH_Q_TYPING != 0,
        skip_top: q.flags & MAILSEARCH_Q_SKIP_TOP != 0,
        skip_list: q.flags & MAILSEARCH_Q_SKIP_LIST != 0,
        relevance: q.flags & MAILSEARCH_Q_RELEVANCE != 0,
        collapse: q.flags & MAILSEARCH_Q_NO_COLLAPSE == 0,
        exclude: q.flags & MAILSEARCH_Q_NO_EXCLUDE == 0,
        n_top: q.n_top as usize,
        n_list,
    })
}

fn search_err(e: SearchError) -> i32 {
    match e {
        SearchError::Cancelled => {
            set_last("cancelled");
            MAILSEARCH_E_CANCELLED
        }
        SearchError::Io(m) => {
            set_last(m);
            MAILSEARCH_E_IO
        }
    }
}

fn open_err(e: OpenError) -> i32 {
    let code = match e {
        OpenError::Version(_) => MAILSEARCH_E_VERSION,
        OpenError::Io(_) => MAILSEARCH_E_IO,
        OpenError::Corrupt(_) => MAILSEARCH_E_CORRUPT,
    };
    set_last(e.to_string());
    code
}

/// Writes the normalisation and layout versions this library reads.
///
/// # Safety
/// `norm_version` and `layout_version` must be null or valid for one `u32` write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_versions(
    norm_version: *mut u32,
    layout_version: *mut u32,
) -> i32 {
    guard(|| {
        if norm_version.is_null() || layout_version.is_null() {
            return arg("null argument");
        }
        // SAFETY: both pointers are non-null and the caller guarantees they are writable.
        unsafe {
            *norm_version = mailsearch_core::NORM_VERSION;
            *layout_version = mailsearch_core::LAYOUT_VERSION;
        }
        MAILSEARCH_OK
    })
}

/// The active directory number of `root` (`CURRENT` → `idx-N`) without opening it.
///
/// # Safety
/// `root` must be valid for `root_len` bytes; `dir` valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_current(
    root: *const c_char,
    root_len: usize,
    dir: *mut u32,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract.
        let Some(r) = (unsafe { text(root, root_len) }).filter(|r| !r.is_empty()) else {
            return arg("root: null, empty or not UTF-8");
        };
        if dir.is_null() {
            return arg("null dir");
        }
        match mailsearch_core::read_current(Path::new(r)) {
            Some(n) => {
                // SAFETY: non-null and writable (caller contract).
                unsafe { *dir = n };
                MAILSEARCH_OK
            }
            None => {
                set_last(format!("{r}: no valid CURRENT"));
                MAILSEARCH_E_IO
            }
        }
    })
}

/// Opens `root` (`$data/search`) read-only. NULL with `*err` set on failure.
///
/// # Safety
/// `root` must be valid for `root_len` bytes; `err` null or valid for one `int` write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_open(
    root: *const c_char,
    root_len: usize,
    err: *mut i32,
) -> *mut MailsearchHandle {
    let mut out: *mut MailsearchHandle = std::ptr::null_mut();
    let code = guard(|| {
        // SAFETY: caller contract.
        let Some(r) = (unsafe { text(root, root_len) }).filter(|r| !r.is_empty()) else {
            return arg("root: null, empty or not UTF-8");
        };
        match Engine::open(Path::new(r)) {
            Ok(e) => {
                let h = MailsearchHandle {
                    cancel: e.cancel_flag(),
                    poisoned: AtomicBool::new(false),
                    inner: UnsafeCell::new(Inner {
                        e,
                        o: Opts::default(),
                    }),
                };
                out = Box::into_raw(Box::new(h));
                MAILSEARCH_OK
            }
            Err(e) => open_err(e),
        }
    });
    if !err.is_null() {
        // SAFETY: non-null and writable (caller contract).
        unsafe { *err = code };
    }
    out
}

/// Own addresses (own-sent feature); replaces the previous set.
///
/// # Safety
/// `addrs` and `lens` must be valid for `n` elements, each address for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_set_owner(
    g: *mut MailsearchHandle,
    addrs: *const *const c_char,
    lens: *const usize,
    n: usize,
) -> i32 {
    with(g, |h| {
        // SAFETY: caller contract.
        let (Some(a), Some(l)) = (unsafe { slice(addrs, n) }, unsafe { slice(lens, n) }) else {
            return arg("null addrs/lens");
        };
        let mut v = Vec::with_capacity(n);
        for (p, len) in a.iter().zip(l) {
            // SAFETY: caller contract.
            let Some(s) = (unsafe { text(*p, *len) }) else {
                return arg("address: null or not UTF-8");
            };
            v.push(s);
        }
        h.e.set_owner(&v);
        MAILSEARCH_OK
    })
}

/// Excluded message id set, applied inside the collector; replaces the previous set.
///
/// # Safety
/// `mids` must be valid for `n` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_set_excluded(
    g: *mut MailsearchHandle,
    mids: *const u64,
    n: usize,
) -> i32 {
    with(g, |h| {
        // SAFETY: caller contract.
        let Some(m) = (unsafe { slice(mids, n) }) else {
            return arg("null mids");
        };
        let mut set = KeySet::default();
        set.reserve(m.len());
        set.extend(m.iter().copied());
        h.e.set_excluded(set);
        MAILSEARCH_OK
    })
}

fn put_hits(dst: *mut MailsearchHit, cap: usize, src: &[engine::Hit]) -> usize {
    let n = src.len().min(cap);
    for (i, s) in src.iter().take(n).enumerate() {
        let hit = MailsearchHit {
            mid: s.mid,
            search_key: s.skey.min(i64::MAX as u64) as i64,
            score: s.score,
            tiers: s.tiers,
            fields: s.fields,
            copies: s.copies,
        };
        // SAFETY: dst is valid for `cap` elements (checked non-null by the caller when cap > 0).
        unsafe { dst.add(i).write(hit) };
    }
    n
}

/// Runs one query.
///
/// # Safety
/// `q` and `r` must be valid; `r.top`/`r.list` valid for `top_cap`/`list_cap` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_search(
    g: *mut MailsearchHandle,
    q: *const MailsearchQuery,
    r: *mut MailsearchResult,
) -> i32 {
    with(g, |h| {
        // SAFETY: caller contract.
        let (Some(q), Some(r)) = (unsafe { q.as_ref() }, unsafe { r.as_mut() }) else {
            return arg("null query/result");
        };
        r.n_top = 0;
        r.n_list = 0;
        r.count = 0;
        r.count_capped = 0;
        r.flags = 0;
        if (r.top.is_null() && r.top_cap > 0) || (r.list.is_null() && r.list_cap > 0) {
            return arg("null result array");
        }
        if q.flags & MAILSEARCH_Q_SKIP_TOP == 0 && (r.top_cap as u64) < q.n_top as u64 {
            return arg("top_cap < n_top");
        }
        // SAFETY: caller contract.
        let query = match unsafe { to_query(q, r.list_cap) } {
            Ok(x) => x,
            Err(c) => return c,
        };
        let hits = match h.e.search(&query, &h.o) {
            Ok(x) => x,
            Err(e) => return search_err(e),
        };
        r.n_top = put_hits(r.top, r.top_cap, &hits.top);
        let list = if query.relevance {
            &hits.rel
        } else {
            &hits.date
        };
        r.n_list = put_hits(r.list, r.list_cap, list);
        r.count = hits.count;
        r.count_capped = hits.count_capped as u32;
        r.flags = if hits.from_recent {
            MAILSEARCH_R_FROM_RECENT
        } else {
            0
        } | if hits.corrected {
            MAILSEARCH_R_CORRECTED
        } else {
            0
        };
        MAILSEARCH_OK
    })
}

/// Makes the running mailsearch_search on `g` return MAILSEARCH_E_CANCELLED. Thread-safe.
///
/// # Safety
/// `g` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_cancel(g: *mut MailsearchHandle) -> i32 {
    guard(|| {
        // SAFETY: only the atomic flag is touched (shared access is sound from any thread).
        let Some(h) = (unsafe { g.as_ref() }) else {
            return arg("null handle");
        };
        h.cancel.store(true, Ordering::Relaxed);
        MAILSEARCH_OK
    })
}

/// People suggestions: correspondents.id best first; returns the number written.
///
/// # Safety
/// `prefix` valid for `len` bytes; `ids` valid for `cap` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_people(
    g: *mut MailsearchHandle,
    prefix: *const c_char,
    len: usize,
    ids: *mut u64,
    cap: usize,
) -> i32 {
    with(g, |h| {
        // SAFETY: caller contract.
        let Some(p) = (unsafe { text(prefix, len) }) else {
            return arg("prefix: null or not UTF-8");
        };
        if ids.is_null() && cap > 0 {
            return arg("null ids");
        }
        let v = match h.e.people(p, cap.min(i32::MAX as usize)) {
            Ok(v) => v,
            Err(e) => return search_err(e),
        };
        for (i, id) in v.iter().enumerate() {
            // SAFETY: i < v.len() <= cap and ids is valid for cap elements.
            unsafe { ids.add(i).write(*id) };
        }
        v.len() as i32
    })
}

/// Byte spans of `text` the query's clauses match; returns the number written.
///
/// # Safety
/// `q` valid; `text` valid for `len` bytes; `spans` valid for `cap` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_highlight(
    g: *mut MailsearchHandle,
    q: *const MailsearchQuery,
    text_p: *const c_char,
    len: usize,
    spans: *mut MailsearchSpan,
    cap: usize,
) -> i32 {
    with(g, |h| {
        // SAFETY: caller contract.
        let Some(q) = (unsafe { q.as_ref() }) else {
            return arg("null query");
        };
        // SAFETY: caller contract.
        let Some(t) = (unsafe { text(text_p, len) }) else {
            return arg("text: null or not UTF-8");
        };
        if spans.is_null() && cap > 0 {
            return arg("null spans");
        }
        if len > u32::MAX as usize {
            return arg("text longer than 4 GiB");
        }
        // SAFETY: caller contract.
        let query = match unsafe { to_query(q, 0) } {
            Ok(x) => x,
            Err(c) => return c,
        };
        let v = h.e.highlight(&query, &h.o, t);
        let n = v.len().min(cap).min(i32::MAX as usize);
        for (i, (s, e, tier)) in v.into_iter().take(n).enumerate() {
            // SAFETY: i < n <= cap and spans is valid for cap elements.
            unsafe {
                spans.add(i).write(MailsearchSpan {
                    start: s,
                    end: e,
                    tier,
                })
            };
        }
        n as i32
    })
}

/// Reopens at the newest commit (or the new directory after a switch) and releases the old
/// mapping's pages. `(*dir, *gen)` is the lease to report as ReaderAt.
///
/// # Safety
/// `dir` and `gen` must be valid for one write each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_reload(
    g: *mut MailsearchHandle,
    dir: *mut u32,
    generation: *mut u64,
) -> i32 {
    with(g, |h| {
        if dir.is_null() || generation.is_null() {
            return arg("null dir/gen");
        }
        match h.e.reload() {
            Ok((d, n)) => {
                // SAFETY: non-null and writable (caller contract).
                unsafe {
                    *dir = d;
                    *generation = n;
                }
                MAILSEARCH_OK
            }
            Err(e) => open_err(e),
        }
    })
}

/// `madvise(MADV_DONTNEED)` every mapping + `malloc_trim`; `*bytes` = mapped bytes advised.
///
/// # Safety
/// `bytes` must be valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_release(g: *mut MailsearchHandle, bytes: *mut u64) -> i32 {
    with(g, |h| {
        if bytes.is_null() {
            return arg("null bytes");
        }
        let n = h.e.release();
        // SAFETY: non-null and writable (caller contract).
        unsafe { *bytes = n };
        MAILSEARCH_OK
    })
}

/// # Safety
/// `n` must be valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_num_docs(g: *mut MailsearchHandle, n: *mut u64) -> i32 {
    with(g, |h| {
        if n.is_null() {
            return arg("null n");
        }
        let v = h.e.num_docs();
        // SAFETY: non-null and writable (caller contract).
        unsafe { *n = v };
        MAILSEARCH_OK
    })
}

/// Frees the handle (also a poisoned one).
///
/// # Safety
/// `g` must be null or a live handle, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_close(g: *mut MailsearchHandle) -> i32 {
    if g.is_null() {
        return arg("null handle");
    }
    guard(|| {
        // SAFETY: a live handle from mailsearch_open (caller contract), freed once.
        drop(unsafe { Box::from_raw(g) });
        MAILSEARCH_OK
    })
}

/// Copies this thread's last error message into `buf` (truncated, NUL-terminated when `len` > 0);
/// returns the message's full length.
///
/// # Safety
/// `buf` must be null or valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mailsearch_last_error(buf: *mut c_char, len: usize) -> usize {
    catch_unwind(|| {
        LAST.with(|l| {
            let s = l.borrow();
            if !buf.is_null() && len > 0 {
                let n = s.len().min(len - 1);
                // SAFETY: buf is valid for len > n bytes (caller contract).
                unsafe {
                    std::ptr::copy_nonoverlapping(s.as_ptr() as *const c_char, buf, n);
                    *buf.add(n) = 0;
                }
            }
            s.len()
        })
    })
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_null_checks() {
        let (mut n, mut l) = (0u32, 0u32);
        // SAFETY: valid stack pointers / nulls.
        unsafe {
            assert_eq!(mailsearch_versions(&mut n, &mut l), MAILSEARCH_OK);
            assert_eq!(
                mailsearch_versions(std::ptr::null_mut(), &mut l),
                MAILSEARCH_E_ARG
            );
        }
        assert_eq!(
            (n, l),
            (
                mailsearch_core::NORM_VERSION,
                mailsearch_core::LAYOUT_VERSION
            )
        );
    }

    #[test]
    fn panics_do_not_escape() {
        assert_eq!(guard(|| panic!("boom")), MAILSEARCH_E_PANIC);
        let mut buf = [0 as c_char; 64];
        // SAFETY: valid buffer.
        let n = unsafe { mailsearch_last_error(buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, "panic: boom".len());
    }

    /// The handle-level panic path (the integration tests reach `with` through a real handle).
    pub(crate) fn panic_in(g: *mut MailsearchHandle) -> i32 {
        with(g, |_| panic!("injected"))
    }

    #[test]
    fn poisoning() {
        crate::tests_ffi::poisoning_with(panic_in);
    }
}

/// Shared by the unit test above and nothing else: builds a handle on a tiny index.
#[cfg(test)]
mod tests_ffi {
    use super::*;

    pub(crate) fn poisoning_with(panic_in: fn(*mut MailsearchHandle) -> i32) {
        let root = std::env::temp_dir().join(format!("gs-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let idx = root.join("idx-1");
        let mail = idx.join(mailsearch_core::MAIL_DIR);
        std::fs::create_dir_all(&mail).unwrap();
        let index = tantivy::Index::create_in_dir(&mail, mailsearch_core::schema()).unwrap();
        mailsearch_core::register(&index);
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        let f = mailsearch_core::Fields::new(&index.schema()).unwrap();
        w.add_document(mailsearch_core::to_tantivy(
            &f,
            &mailsearch_core::DocInput {
                mid: 1,
                search_key: 1 << 20,
                subject: "hello".into(),
                ..Default::default()
            },
        ))
        .unwrap();
        w.commit().unwrap();
        std::fs::write(idx.join("VERSION"), mailsearch_core::stamp()).unwrap();
        std::fs::write(root.join("CURRENT"), "idx-1\n").unwrap();
        let r = root.to_str().unwrap();
        let mut err = 1;
        // SAFETY: valid pointers.
        unsafe {
            let g = mailsearch_open(r.as_ptr() as *const c_char, r.len(), &mut err);
            assert_eq!(err, MAILSEARCH_OK);
            assert!(!g.is_null());
            let mut n = 0u64;
            assert_eq!(mailsearch_num_docs(g, &mut n), MAILSEARCH_OK);
            assert_eq!(n, 1);
            assert_eq!(panic_in(g), MAILSEARCH_E_PANIC);
            assert_eq!(mailsearch_num_docs(g, &mut n), MAILSEARCH_E_POISONED);
            assert_eq!(mailsearch_cancel(g), MAILSEARCH_OK);
            assert_eq!(mailsearch_close(g), MAILSEARCH_OK);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
