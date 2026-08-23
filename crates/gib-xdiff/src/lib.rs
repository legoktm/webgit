//! Line-level diffing, by way of git's own xdiff.
//!
//! git's diff output is not simply "a minimal edit script": xdiff runs Myers
//! with cost and snake heuristics that deliberately give up minimality on large
//! inputs, then canonicalises the result with `xdl_change_compact`, which
//! slides each run of changed lines as far as it can and merges runs that meet.
//! Two diffs can both be minimal and still disagree about *which* lines
//! changed, and that choice is exactly what a blame annotation reports. Any
//! reimplementation therefore diverges from git somewhere; the only way to
//! agree everywhere is to run the same code, so this crate compiles the
//! vendored `vendor/xdiff` and calls it.
//!
//! # Usage
//!
//! [`hunks`] reports the changed line ranges and nothing else — the primitive
//! blame walks a file's history with. [`unified`] renders a unified diff body,
//! hunk headers included, which is what a patch is built from.
//!
//! ```
//! # use gib_xdiff::hunks;
//! let before = b"context\nold\ntrailer\n";
//! let after = b"context\nnew\ntrailer\n";
//! let changed = hunks(before, after).unwrap();
//! assert_eq!(changed.len(), 1);
//! assert_eq!(changed[0].before, 1..2);
//! assert_eq!(changed[0].after, 1..2);
//! ```

#![deny(clippy::all)]

mod shim;

use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use std::fmt;
use std::ops::Range;

// ---------------------------------------------------------------------------
// The C surface
// ---------------------------------------------------------------------------
//
// These mirror `vendor/xdiff/xdiff.h`. Field order and count matter: the C
// writes through pointers into these, so a struct that is short by a field
// hands xdiff someone else's stack. `xdemitcb_t` in particular has three
// fields, not the two a reader might expect from how few we set.

#[repr(C)]
struct MmFile {
    ptr: *mut c_char,
    size: c_long,
}

#[repr(C)]
struct MmBuffer {
    ptr: *mut c_char,
    size: c_long,
}

#[repr(C)]
struct XpParam {
    flags: c_ulong,
    ignore_regex: *mut c_void,
    ignore_regex_nr: usize,
    anchors: *mut *mut c_char,
    anchors_nr: usize,
}

type HunkFn = extern "C" fn(c_long, c_long, c_long, c_long, *mut c_void) -> c_int;
type OutLineFn = extern "C" fn(*mut c_void, *mut MmBuffer, c_int) -> c_int;

#[repr(C)]
struct XdEmitConf {
    ctxlen: c_long,
    interhunkctxlen: c_long,
    flags: c_ulong,
    find_func: *mut c_void,
    find_func_priv: *mut c_void,
    hunk_func: Option<HunkFn>,
}

#[repr(C)]
struct XdEmitCb {
    priv_: *mut c_void,
    out_hunk: *mut c_void,
    out_line: Option<OutLineFn>,
}

/// `xdiff.h`'s `XDL_EMIT_FUNCNAMES`: append the enclosing function's line to
/// each `@@` header, as git does.
const XDL_EMIT_FUNCNAMES: c_ulong = 1 << 0;

unsafe extern "C" {
    fn xdl_diff(
        mf1: *mut MmFile,
        mf2: *mut MmFile,
        xpp: *const XpParam,
        xecfg: *const XdEmitConf,
        ecb: *mut XdEmitCb,
    ) -> c_int;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// One run of changed lines, as the two sides' line ranges.
///
/// Ranges are zero-based and end-exclusive. An insertion has an empty
/// [`before`](Hunk::before) and a deletion an empty [`after`](Hunk::after);
/// the empty range still carries the position the change sits at on that side,
/// which is what makes it usable for line attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The lines this hunk replaces in the "before" file.
    pub before: Range<usize>,
    /// The lines it replaces them with in the "after" file.
    pub after: Range<usize>,
}

/// xdiff could not produce a diff. In practice this only happens when an
/// allocation fails, which for a browser build means the tab is out of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffFailed;

impl fmt::Display for DiffFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("xdiff could not compute a diff")
    }
}

impl std::error::Error for DiffFailed {}

/// The changed line ranges between two files, with no context.
///
/// This is the shape git's blame consumes (`blame.c`'s `diff_hunks`, which
/// passes no flags and collects hunks through the same callback), and the
/// reason it is a separate entry point from [`unified`]: blame wants the
/// ranges, never the text.
pub fn hunks(before: &[u8], after: &[u8]) -> Result<Vec<Hunk>, DiffFailed> {
    extern "C" fn collect(
        start_a: c_long,
        count_a: c_long,
        start_b: c_long,
        count_b: c_long,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: `data` is the `Vec<Hunk>` handed to xdiff as `ecb.priv_`,
        // borrowed for the duration of the `xdl_diff` call below.
        let out = unsafe { &mut *data.cast::<Vec<Hunk>>() };
        let (start_a, count_a) = (start_a as usize, count_a as usize);
        let (start_b, count_b) = (start_b as usize, count_b as usize);
        out.push(Hunk {
            before: start_a..start_a + count_a,
            after: start_b..start_b + count_b,
        });
        0
    }

    let mut out: Vec<Hunk> = Vec::new();
    let mut emit = XdEmitConf {
        hunk_func: Some(collect),
        ..emit_conf(0)
    };
    // `hunk_func` bypasses the emitter entirely, so no `out_line` is needed.
    let mut cb = XdEmitCb {
        priv_: std::ptr::from_mut(&mut out).cast(),
        out_hunk: std::ptr::null_mut(),
        out_line: None,
    };
    run(before, after, &mut emit, &mut cb)?;
    Ok(out)
}

/// A unified diff of the two files, with `context` lines around each hunk.
///
/// The result is the body of a diff — `@@` headers and their lines — without
/// the `---`/`+++` file headers, which name paths this crate never sees. Each
/// `@@` header carries git's enclosing-function suffix, because it is written
/// by the same code that writes git's.
///
/// A line without a trailing newline is followed by xdiff's own
/// `\ No newline at end of file` marker (`xutils.c`), so a caller assembling a
/// patch does not have to add one.
pub fn unified(before: &[u8], after: &[u8], context: usize) -> Result<Vec<u8>, DiffFailed> {
    extern "C" fn write_line(data: *mut c_void, mb: *mut MmBuffer, nbuf: c_int) -> c_int {
        // SAFETY: `data` is the output `Vec<u8>`, and xdiff hands us `nbuf`
        // buffers, each describing `size` readable bytes at `ptr`.
        unsafe {
            let out = &mut *data.cast::<Vec<u8>>();
            for i in 0..nbuf as usize {
                let buf = &*mb.add(i);
                if buf.ptr.is_null() || buf.size <= 0 {
                    continue;
                }
                out.extend_from_slice(std::slice::from_raw_parts(
                    buf.ptr.cast::<u8>(),
                    buf.size as usize,
                ));
            }
        }
        0
    }

    let mut out: Vec<u8> = Vec::new();
    let mut emit = XdEmitConf {
        // git sets this for every diff it prints; without it xdiff emits a bare
        // `@@ -a,b +c,d @@` and the enclosing-function suffix is silently lost.
        flags: XDL_EMIT_FUNCNAMES,
        ..emit_conf(context as c_long)
    };
    // With `out_hunk` left null, xdiff formats the `@@` header itself and sends
    // it through `out_line` (`xdl_emit_hunk_hdr`), which is what we want: the
    // header comes out already carrying its function-context suffix.
    let mut cb = XdEmitCb {
        priv_: std::ptr::from_mut(&mut out).cast(),
        out_hunk: std::ptr::null_mut(),
        out_line: Some(write_line),
    };
    run(before, after, &mut emit, &mut cb)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// A zeroed emit configuration with `ctxlen` set, matching how git initialises
/// one before overriding just the fields it cares about.
fn emit_conf(ctxlen: c_long) -> XdEmitConf {
    XdEmitConf {
        ctxlen,
        interhunkctxlen: 0,
        flags: 0,
        find_func: std::ptr::null_mut(),
        find_func_priv: std::ptr::null_mut(),
        hunk_func: None,
    }
}

/// Drive `xdl_diff` over two byte slices.
///
/// `flags` is left at zero throughout: that is what git's blame passes, and it
/// selects Myers with no whitespace handling and no indent heuristic. Changing
/// it changes which lines a diff reports, so it is deliberately not a knob.
fn run(
    before: &[u8],
    after: &[u8],
    emit: &mut XdEmitConf,
    cb: &mut XdEmitCb,
) -> Result<(), DiffFailed> {
    // `mmfile_t` is not const in xdiff's signatures, but xdiff only ever reads
    // the bytes: it builds its own record table in `xdl_prepare` and never
    // writes back. Casting away the shared reference here avoids copying both
    // blobs, which for a browser tab diffing a large file is the difference
    // between one copy in memory and three.
    let mut f1 = MmFile {
        ptr: before.as_ptr().cast::<c_char>().cast_mut(),
        size: before.len() as c_long,
    };
    let mut f2 = MmFile {
        ptr: after.as_ptr().cast::<c_char>().cast_mut(),
        size: after.len() as c_long,
    };
    let params = XpParam {
        flags: 0,
        ignore_regex: std::ptr::null_mut(),
        ignore_regex_nr: 0,
        anchors: std::ptr::null_mut(),
        anchors_nr: 0,
    };

    // SAFETY: every pointer handed over is valid for the call, the structs
    // match `xdiff.h` field for field, and the callbacks only touch the
    // `priv_` we installed.
    let rc = unsafe { xdl_diff(&mut f1, &mut f2, &params, emit, cb) };
    if rc == 0 { Ok(()) } else { Err(DiffFailed) }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod differential;
