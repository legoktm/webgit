/*
 * The indirection header xdiff includes to reach its host application.
 *
 * `vendor/xdiff` ships its own copy of this file, written for a host with a
 * libc. We have neither a libc (wasm32-unknown-unknown has no sysroot) nor any
 * wish to patch the submodule, so this file is force-included ahead of it with
 * `-include`. It claims the vendored copy's include guard, `__GIT_XDIFF_H__`,
 * which makes that copy expand to nothing when `xinclude.h` reaches it.
 *
 * Note that `-I` ordering alone cannot do this: `xinclude.h` reaches for its
 * header with a quoted include, and a quoted include resolves relative to the
 * including file's own directory first, so the vendored copy would always win.
 */

#ifndef __GIT_XDIFF_H__
#define __GIT_XDIFF_H__

/* clang keeps providing these in a freestanding build; they define nothing
 * that needs a hosted environment. */
#include <stddef.h>
#include <stdint.h>
#include <limits.h>

/*
 * Allocation is routed to the Rust allocator (see `src/shim.rs`)
 */
void *gib_xdiff_malloc(size_t size);
void *gib_xdiff_calloc(size_t nmemb, size_t size);
void *gib_xdiff_realloc(void *ptr, size_t size);
void gib_xdiff_free(void *ptr);
void gib_xdiff_bug(const char *msg);

#define xdl_malloc(x)		gib_xdiff_malloc(x)
#define xdl_calloc(n, sz)	gib_xdiff_calloc(n, sz)
#define xdl_realloc(ptr, x)	gib_xdiff_realloc(ptr, x)
#define xdl_free(ptr)		gib_xdiff_free(ptr)

/* xdiff calls this only on "can't happen" invariant failures, where git would
 * die(). Panicking is the closest equivalent that keeps us out of undefined
 * behaviour. */
#define XDL_BUG(msg)		gib_xdiff_bug(msg)

/*
 * Rust's compiler-builtins supplies memcpy/memset/memcmp on wasm, but not the
 * four below; `src/shim.rs` provides those for wasm targets and lets the
 * platform libc provide them everywhere else.
 */
void *memcpy(void *dest, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *s1, const void *s2, size_t n);
void *memchr(const void *s, int c, size_t n);
size_t strlen(const char *s);
int strncmp(const char *s1, const char *s2, size_t n);

/*
 * Only `xemit.c`'s "does this line look like the start of a function" test
 * uses these, and only for ASCII. The C locale's definitions are all xdiff
 * ever relied on, so spelling them out costs nothing and drops the ctype.h
 * dependency.
 */
#define isspace(c) ((c) == ' ' || ((unsigned)(c) - '\t') < 5u)
#define isalpha(c) ((((unsigned)(c) | 32u) - 'a') < 26u)

#define XDL_UNUSED __attribute__((unused))

/*
 * `-I<regex>` support, which we do not expose: `xpparam_t::ignore_regex` is
 * always left null, so `record_matches_regex` is never reached. Defining the
 * types away avoids a regex.h dependency. Stubbing this as a macro rather than
 * an inline function keeps the pointer types from having to line up, which
 * they cannot once `xdl_regex_t` is `void *`.
 */
#define xdl_regex_t void *
#define xdl_regmatch_t void *
#define xdl_regexec_buf(preg, buf, size, nmatch, pmatch, eflags) (15 /* REG_ASSERT */)

#endif /* __GIT_XDIFF_H__ */
