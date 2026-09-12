/*
 * The host header `diff-delta.c` includes to reach its application.
 */

#ifndef GIT_COMPAT_UTIL_H
#define GIT_COMPAT_UTIL_H

/* clang keeps providing these in a freestanding build; they define nothing
 * that needs a hosted environment. */
#include <stddef.h>
#include <stdint.h>

/* `create_delta` tracks its output position in an `off_t`. Signed and at least
 * as wide as a pointer is all it needs of the type. */
typedef long off_t;

/*
 * Allocation is routed to the Rust allocator (see `src/shim.rs`)
 */
void *gib_delta_malloc(size_t size);
void *gib_delta_calloc(size_t nmemb, size_t size);
void *gib_delta_realloc(void *ptr, size_t size);
void gib_delta_free(void *ptr);

#define malloc(size)		gib_delta_malloc(size)
#define calloc(n, size)		gib_delta_calloc(n, size)
#define realloc(ptr, size)	gib_delta_realloc(ptr, size)
#define free(ptr)		gib_delta_free(ptr)

void *memset(void *s, int c, size_t n);

/* git's zeroing helper, introduced by the very commit this file was taken at. */
#define MEMZERO_ARRAY(array, nr) memset((array), 0, (nr) * sizeof(*(array)))

#define FLEX_ARRAY

/*
 * `create_delta_index` asserts once, on an invariant its own bookkeeping is
 * meant to guarantee. Reaching it means the index is malformed, so aborting is
 * the honest answer; `gib_delta_bug` is an `extern "C"` Rust function, and a
 * panic across that boundary aborts rather than unwinding into C.
 */
void gib_delta_bug(const char *msg);
#define assert(expr) \
	do { if (!(expr)) gib_delta_bug("diff-delta.c: " #expr); } while (0)

/* Set by git's build for `-Wsign-compare`; the vendored file names it. */
#define DISABLE_SIGN_COMPARE_WARNINGS

#endif /* GIT_COMPAT_UTIL_H */
