//! The host facilities `diff-delta.c` expects, supplied without a libc.
//!
//! `shim/git-compat-util.h` redirects the encoder's `malloc` family here, so
//! allocation goes through Rust's allocator on every target — which on wasm is
//! the only allocator there is. The same arrangement as [`gib_xdiff`]'s shim,
//! and the header layout below is the same trick for the same reason: Rust's
//! allocator wants the block's layout back at free time and C does not carry
//! one.
//!
//! Unlike that shim there are no string functions here. The encoder needs only
//! `memset`, which Rust's `compiler_builtins` already provides on wasm and the
//! platform libc provides elsewhere — so nothing is defined for it, and none of
//! the "must stay gated to wasm" hazards apply.
//!
//! [`gib_xdiff`]: https://docs.rs/gib-xdiff

use std::alloc::{Layout, alloc, dealloc, realloc};

/// Alignment for every block handed to the encoder. 16 covers any type it
/// stores and leaves room for the size header below.
const ALIGN: usize = 16;

/// Each allocation is prefixed with its own total size, so [`gib_delta_free`]
/// can reconstruct the layout. The header is `ALIGN` bytes so the pointer
/// handed to C keeps that alignment.
const HEADER: usize = ALIGN;

/// The layout used for a block of `total` bytes, header included.
fn layout(total: usize) -> Layout {
    Layout::from_size_align(total, ALIGN).expect("delta allocation size overflows a Layout")
}

/// # Safety
///
/// Returns a pointer suitable for `gib_delta_free`/`gib_delta_realloc` only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_delta_malloc(size: usize) -> *mut u8 {
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
/// See [`gib_delta_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_delta_calloc(nmemb: usize, size: usize) -> *mut u8 {
    let Some(bytes) = nmemb.checked_mul(size) else {
        return std::ptr::null_mut();
    };
    // SAFETY: forwarding to our own allocator.
    let ptr = unsafe { gib_delta_malloc(bytes) };
    if !ptr.is_null() {
        // SAFETY: `ptr` owns `bytes` writable bytes.
        unsafe { std::ptr::write_bytes(ptr, 0, bytes) };
    }
    ptr
}

/// # Safety
///
/// `ptr` must be null or have come from [`gib_delta_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_delta_realloc(ptr: *mut u8, size: usize) -> *mut u8 {
    if ptr.is_null() {
        // SAFETY: forwarding to our own allocator.
        return unsafe { gib_delta_malloc(size) };
    }
    let Some(new_total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `ptr` came from `gib_delta_malloc`, so the header sits just below
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
/// `ptr` must be null or have come from [`gib_delta_malloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_delta_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: as for `gib_delta_realloc`.
    unsafe {
        let base = ptr.sub(HEADER);
        let total = base.cast::<usize>().read();
        dealloc(base, layout(total));
    }
}

/// The encoder's one `assert`, on an invariant its own bookkeeping is meant to
/// guarantee. Reaching it means the index is malformed, and continuing would
/// read past the end of it.
///
/// Panicking out of an `extern "C"` function aborts rather than unwinding into
/// C, which is the defined behaviour we want here.
///
/// # Safety
///
/// Called only by the encoder, which passes a static C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gib_delta_bug(_msg: *const u8) -> ! {
    panic!("diff-delta.c reported a violated internal invariant");
}
