/*
 * The half of git's `delta.h` that `diff-delta.c` defines.
 */

#ifndef DELTA_H
#define DELTA_H

struct delta_index;

/*
 * Compute index data from the given buffer.
 *
 * The buffer must not be freed nor altered before `free_delta_index` is called;
 * the index points into it. `gib-diff-delta`'s `DeltaIndex` is what holds
 * webgit to that.
 */
extern struct delta_index *create_delta_index(const void *buf, unsigned long bufsize);

extern void free_delta_index(struct delta_index *index);

extern unsigned long sizeof_delta_index(struct delta_index *index);

/*
 * Create a delta rebuilding `trg_buf` from the buffer `index` was built over.
 * Returns NULL if it would exceed `max_size` (when that is non-zero), and
 * otherwise a buffer the caller frees, with its length in `delta_size`.
 */
extern void *create_delta(const struct delta_index *index,
			  const void *trg_buf, unsigned long trg_size,
			  unsigned long *delta_size, unsigned long max_size);

#endif /* DELTA_H */
