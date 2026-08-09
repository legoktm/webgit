use std::cell::RefCell;
use std::cmp::min;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::{File, FileSystemError, Offset};

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE_U64: u64 = 4096;

/// A page cache that can be shared between successive [`CachingPageReader`]s
/// over the same file, so pages fetched by one lookup are reused by the next.
///
/// Pages are stored as `Rc<[u8]>` and borrows of the map are never held across
/// an `await`, so concurrently-polled reads (e.g. via `join_all`) can share a
/// cache without risking a `RefCell` double-borrow panic. The worst case for a
/// race is two reads fetching the same page, which is merely redundant.
pub type PageCache = Rc<RefCell<BTreeMap<Offset, Rc<[u8]>>>>;

/// Create an empty [`PageCache`].
pub fn new_page_cache() -> PageCache {
    Rc::new(RefCell::new(BTreeMap::new()))
}

/// A [`File`] wrapper that reads in 4 KiB pages and caches them, so repeated
/// or overlapping reads of the same region cost one underlying read.
pub struct CachingPageReader<F> {
    file: F,
    pages: PageCache,
}

impl<F: File> CachingPageReader<F> {
    /// Create a reader with its own private page cache.
    pub fn new(file: F) -> Self {
        Self {
            file,
            pages: new_page_cache(),
        }
    }

    /// Create a reader backed by an existing, shared page cache.
    pub fn with_cache(file: F, pages: PageCache) -> Self {
        Self { file, pages }
    }

    fn cached_page(&self, page_offset: Offset) -> Option<Rc<[u8]>> {
        self.pages.borrow().get(&page_offset).cloned()
    }

    async fn get_page(&mut self, page_offset: Offset) -> Result<Rc<[u8]>, FileSystemError> {
        if let Some(page) = self.cached_page(page_offset) {
            return Ok(page);
        }
        let mut page = vec![0u8; PAGE_SIZE];
        let read_len = self.file.read_segment(page_offset, &mut page).await?;
        page.truncate(read_len);
        let page: Rc<[u8]> = Rc::from(page.into_boxed_slice());
        // Borrow only to insert; never across the await above.
        self.pages.borrow_mut().insert(page_offset, page.clone());
        Ok(page)
    }

    /// Ensure every page spanning `[first_page, last_page]` (both page-aligned)
    /// is cached, fetching the contiguous run of missing pages in a single
    /// underlying read. For large sequential reads this collapses what would
    /// otherwise be one fetch per page into one request.
    async fn ensure_pages(
        &mut self,
        first_page: Offset,
        last_page: Offset,
    ) -> Result<(), FileSystemError> {
        // Find the contiguous run of missing pages under a short borrow, then
        // drop it before the await below.
        let (start, last_missing) = {
            let pages = self.pages.borrow();
            let mut first_missing: Option<Offset> = None;
            let mut last_missing = first_page;
            let mut page = first_page;
            while page <= last_page {
                if !pages.contains_key(&page) {
                    first_missing.get_or_insert(page);
                    last_missing = page;
                }
                page = page + PAGE_SIZE_U64;
            }
            match first_missing {
                Some(start) => (start, last_missing),
                None => return Ok(()),
            }
        };
        // One read covering the whole missing run. Any already-cached pages
        // caught in the middle are simply re-read and overwritten with the same
        // bytes, which is rare for the sequential reads this optimises.
        let span_len = usize::try_from(last_missing.0 - start.0).unwrap() + PAGE_SIZE;
        let mut buf = vec![0u8; span_len];
        let read_len = self.file.read_segment(start, &mut buf).await?;
        buf.truncate(read_len);
        let mut page_offset = start;
        let mut pages = self.pages.borrow_mut();
        for chunk in buf.chunks(PAGE_SIZE) {
            pages.insert(page_offset, Rc::from(chunk.to_vec().into_boxed_slice()));
            page_offset = page_offset + PAGE_SIZE_U64;
        }
        Ok(())
    }
}

impl<F: File> File for CachingPageReader<F> {
    async fn read_all(&mut self) -> Result<Vec<u8>, FileSystemError> {
        self.file.read_all().await
    }

    async fn read_segment(
        &mut self,
        offset: Offset,
        dest: &mut [u8],
    ) -> Result<usize, FileSystemError> {
        if dest.is_empty() {
            return Ok(0);
        }
        let mut page_offset = (offset / PAGE_SIZE_U64) * PAGE_SIZE_U64;
        // Fetch all pages this read spans in one go before copying them out.
        let last_byte = offset.0 + (dest.len() as u64) - 1;
        let last_page = Offset((last_byte / PAGE_SIZE_U64) * PAGE_SIZE_U64);
        self.ensure_pages(page_offset, last_page).await?;
        let mut page_start = usize::try_from(offset.0 - page_offset.0).unwrap();
        let mut dest_pos = 0;
        while dest_pos < dest.len() {
            let page = self.get_page(page_offset).await?;
            let page_end = min(page.len(), page_start + dest.len() - dest_pos);
            let dest_end = dest_pos + page_end - page_start;
            dest[dest_pos..dest_end].copy_from_slice(&page[page_start..page_end]);
            dest_pos = dest_end;
            if page.len() < PAGE_SIZE {
                break;
            }
            page_start = 0;
            page_offset = page_offset + PAGE_SIZE_U64;
        }
        Ok(dest_pos)
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    impl<T: AsRef<[u8]>> File for Cursor<T> {
        async fn read_all(&mut self) -> Result<Vec<u8>, FileSystemError> {
            self.seek(SeekFrom::Start(0)).unwrap();
            let mut out = Vec::new();
            self.read_to_end(&mut out).unwrap();
            Ok(out)
        }

        async fn read_segment(
            &mut self,
            offset: Offset,
            dest: &mut [u8],
        ) -> Result<usize, FileSystemError> {
            let available_len = u64::try_from(self.get_ref().as_ref().len()).unwrap() - offset.0;
            let read_len = min(usize::try_from(available_len).unwrap(), dest.len());
            self.seek(SeekFrom::Start(offset.0)).unwrap();
            self.read_exact(&mut dest[0..(read_len)]).unwrap();
            Ok(read_len)
        }
    }

    #[test]
    fn read_whole_page() {
        let mut buf = Vec::with_capacity(4 * PAGE_SIZE);
        for i in 0..4u8 {
            buf.extend_from_slice(&[i; PAGE_SIZE]);
        }
        let cur = Cursor::new(buf);
        let mut buf = [0u8; PAGE_SIZE];
        let mut reader = CachingPageReader::new(cur);
        block_on(reader.read_segment(Offset(2 * PAGE_SIZE_U64), &mut buf)).unwrap();
        assert!(buf.iter().all(|b| *b == 2));
    }

    #[test]
    fn read_across_page_boundary() {
        let mut buf = Vec::with_capacity(4 * PAGE_SIZE);
        for i in 0..4u8 {
            buf.extend_from_slice(&[i; PAGE_SIZE]);
        }
        let cur = Cursor::new(buf);
        let mut buf = [0u8; PAGE_SIZE];
        let mut reader = CachingPageReader::new(cur);
        block_on(reader.read_segment(Offset(PAGE_SIZE_U64 + PAGE_SIZE_U64 / 2), &mut buf)).unwrap();
        assert!(&buf[0..PAGE_SIZE / 2].iter().all(|b| *b == 1));
        assert!(&buf[PAGE_SIZE / 2..].iter().all(|b| *b == 2));
    }

    #[test]
    fn read_across_many_pages() {
        let mut buf = Vec::with_capacity(4 * PAGE_SIZE);
        for i in 0..4u8 {
            buf.extend_from_slice(&[i; PAGE_SIZE]);
        }
        let cur = Cursor::new(buf);
        let mut buf = [0u8; 2 * PAGE_SIZE];
        let mut reader = CachingPageReader::new(cur);
        block_on(reader.read_segment(Offset(PAGE_SIZE_U64 + PAGE_SIZE_U64 / 2), &mut buf)).unwrap();
        let mut expected = Vec::with_capacity(2 * PAGE_SIZE);
        expected.extend_from_slice(&[1u8; PAGE_SIZE / 2]);
        expected.extend_from_slice(&[2u8; PAGE_SIZE]);
        expected.extend_from_slice(&[3u8; PAGE_SIZE / 2]);
        assert_eq!(buf.as_slice(), &expected);
    }

    #[test]
    fn read_last_segment() {
        let mut buf = Vec::with_capacity(PAGE_SIZE + 1);
        buf.extend_from_slice(&[1u8; PAGE_SIZE]);
        buf.push(2);
        let cur = Cursor::new(buf);
        let mut buf = [0u8; 4];
        let mut reader = CachingPageReader::new(cur);
        let read_len = block_on(reader.read_segment(Offset(PAGE_SIZE_U64 - 2), &mut buf)).unwrap();
        assert_eq!(read_len, 3);
        assert_eq!(buf, [1, 1, 2, 0]);
    }
}
