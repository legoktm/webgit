use std::ops::Range;

pub trait SubsliceRange {
    fn subslice_range_stable(&self, subslice: &Self) -> Option<Range<usize>>;
}

impl<T> SubsliceRange for [T] {
    fn subslice_range_stable(&self, subslice: &[T]) -> Option<Range<usize>> {
        let start = match subslice.first() {
            Some(first) => self.element_offset(first)?,
            // An empty subslice has no element for `element_offset` to locate,
            // and empty fields are ordinary in git objects: a commit written
            // with `--allow-empty-message` has an empty message, and an author
            // line may carry an empty name. Find it by address instead, the
            // way the unstable `slice::subslice_range` does.
            None => empty_subslice_start(self, subslice)?,
        };
        let end = start + subslice.len();
        Some(Range { start, end })
    }
}

/// The index at which an empty `subslice` of `slice` starts, or `None` if it
/// does not point into `slice`.
///
/// An offset of exactly `slice.len()` counts as inside: the empty message of a
/// commit that has none sits at the very end of the object body.
fn empty_subslice_start<T>(slice: &[T], subslice: &[T]) -> Option<usize> {
    let size = size_of::<T>();
    if size == 0 {
        return None;
    }
    let offset = subslice
        .as_ptr()
        .addr()
        .checked_sub(slice.as_ptr().addr())?;
    if offset % size != 0 {
        return None;
    }
    let start = offset / size;
    (start <= slice.len()).then_some(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_subslice() {
        let data = b"a commit message";
        assert_eq!(data.subslice_range_stable(&data[2..8]), Some(2..8));
        assert_eq!(data.subslice_range_stable(data.as_slice()), Some(0..16));
    }

    /// Empty subslices have no element to locate, so they are found by
    /// address, including one at the very end of the slice.
    #[test]
    fn empty_subslice() {
        let data = b"a commit message";
        assert_eq!(data.subslice_range_stable(&data[0..0]), Some(0..0));
        assert_eq!(data.subslice_range_stable(&data[7..7]), Some(7..7));
        assert_eq!(data.subslice_range_stable(&data[16..16]), Some(16..16));
    }

    #[test]
    fn foreign_slice() {
        let data = b"a commit message";
        let other = b"another buffer!!";
        assert_eq!(data.subslice_range_stable(other.as_slice()), None);
    }

    /// Elements wider than a byte are counted in elements, not bytes.
    #[test]
    fn wide_elements() {
        let data: [u32; 4] = [1, 2, 3, 4];
        assert_eq!(data.subslice_range_stable(&data[1..3]), Some(1..3));
        assert_eq!(data.subslice_range_stable(&data[3..3]), Some(3..3));
    }

    /// Zero-sized elements carry no position, so there is nothing to report.
    #[test]
    fn zero_sized_elements() {
        let data: [(); 4] = [(); 4];
        assert_eq!(data.subslice_range_stable(&data[2..2]), None);
    }
}
