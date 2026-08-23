//! The host facilities xdiff expects to find, supplied without a libc.
//!
//! `shim/git-xdiff.h` routes xdiff's `xdl_malloc` family here, so allocation
//! goes through Rust's allocator on every target. Rust's `compiler_builtins`
//! already provides `memcpy`, `memset` and `memcmp` for
//! `wasm32-unknown-unknown`, but not `memchr`, `strlen` or `strncmp`, so those
//! three are defined here for wasm only — see the warning on them below.

use std::alloc::{Layout, alloc, dealloc, realloc};
#[cfg(target_arch = "wasm32")]
use std::ffi::{c_char, c_int, c_void};

/// Alignment for every block handed to xdiff. 16 covers any type xdiff stores
/// and leaves room for the size header below.
const ALIGN: usize = 16;

/// Rust's allocator needs the block's layout back at free time and C does not
/// carry one, so each allocation is prefixed with its own total size. The
/// header is `ALIGN` bytes so the pointer handed to C keeps that alignment.
const HEADER: usize = ALIGN;

/// The layout used for a block of `total` bytes, header included.
fn layout(total: usize) -> Layout {
    Layout::from_size_align(total, ALIGN).expect("xdiff allocation size overflows a Layout")
}

/// # Safety
///
/// Returns a pointer suitable for `gib_xdiff_free`/`gib_xdiff_realloc` only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_xdiff_malloc(size: usize) -> *mut u8 {
    let Some(total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `total` is non-zero, since it is at least HEADER.
    let base = unsafe { alloc(layout(total)) };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `base` is freshly allocated with room and alignment for a usize.
    unsafe {
        base.cast::<usize>().write(total);
        base.add(HEADER)
    }
}

/// # Safety
///
/// See [`gib_xdiff_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_xdiff_calloc(nmemb: usize, size: usize) -> *mut u8 {
    let Some(bytes) = nmemb.checked_mul(size) else {
        return std::ptr::null_mut();
    };
    // SAFETY: forwarding to our own allocator.
    let ptr = unsafe { gib_xdiff_malloc(bytes) };
    if !ptr.is_null() {
        // SAFETY: `ptr` owns `bytes` writable bytes.
        unsafe { std::ptr::write_bytes(ptr, 0, bytes) };
    }
    ptr
}

/// # Safety
///
/// `ptr` must be null or have come from [`gib_xdiff_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_xdiff_realloc(ptr: *mut u8, size: usize) -> *mut u8 {
    if ptr.is_null() {
        // SAFETY: forwarding to our own allocator.
        return unsafe { gib_xdiff_malloc(size) };
    }
    let Some(new_total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `ptr` came from `gib_xdiff_malloc`, so the header sits just below
    // it and records the block's current total size.
    unsafe {
        let base = ptr.sub(HEADER);
        let old_total = base.cast::<usize>().read();
        let grown = realloc(base, layout(old_total), new_total);
        if grown.is_null() {
            return std::ptr::null_mut();
        }
        grown.cast::<usize>().write(new_total);
        grown.add(HEADER)
    }
}

/// # Safety
///
/// `ptr` must be null or have come from [`gib_xdiff_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_xdiff_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: as for `gib_xdiff_realloc`.
    unsafe {
        let base = ptr.sub(HEADER);
        let total = base.cast::<usize>().read();
        dealloc(base, layout(total));
    }
}

/// xdiff's `XDL_BUG`: an invariant it considers impossible has been violated.
/// git calls `die()` here; a panic is the closest thing that does not continue
/// into undefined behaviour.
///
/// # Safety
///
/// Called only by xdiff, which passes a static C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_xdiff_bug(_msg: *const u8) -> ! {
    panic!("xdiff reported a violated internal invariant");
}

// ---------------------------------------------------------------------------
// String functions, for wasm only
// ---------------------------------------------------------------------------
//
// These MUST stay gated to wasm. `#[unsafe(no_mangle)]` puts them in the global
// symbol namespace, so on a hosted target they interpose over the platform
// libc's versions for the whole process — every Rust dependency included. The
// failure that produces is silent and looks nothing like its cause.

/// # Safety
///
/// `s` must point to `n` readable bytes.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memchr(s: *const c_void, c: c_int, n: usize) -> *mut c_void {
    // SAFETY: the caller guarantees `n` readable bytes at `s`.
    let haystack = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), n) };
    match haystack.iter().position(|&b| b == c as u8) {
        // SAFETY: `i` is an index within `haystack`.
        Some(i) => unsafe { s.cast::<u8>().add(i).cast_mut().cast() },
        None => std::ptr::null_mut(),
    }
}

/// # Safety
///
/// `s` must point to a NUL-terminated string.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let mut n = 0;
    // SAFETY: the caller guarantees a NUL terminator.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

/// # Safety
///
/// `a` and `b` must each point to `n` readable bytes, or to a NUL-terminated
/// string shorter than that.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
    for i in 0..n {
        // SAFETY: the caller guarantees both are readable through `i`.
        let (x, y) = unsafe { (*a.add(i) as u8, *b.add(i) as u8) };
        if x != y {
            return c_int::from(x) - c_int::from(y);
        }
        if x == 0 {
            break;
        }
    }
    0
}
