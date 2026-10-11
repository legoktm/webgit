use std::cell::{Cell, RefCell};
use std::cmp::{max, min};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::lru::ByteLru;
use crate::{File, FileSystemError, Offset};

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE_U64: u64 = 4096;

/// A page cache that can be shared between successive [`CachingPageReader`]s
/// over the same file, so pages fetched by one lookup are reused by the next.
///
/// Pages are stored as `Rc<[u8]>` and borrows of the cache are never held
/// across an `await`, so concurrently-polled reads (e.g. via `join_all`) can
/// share a cache without risking a `RefCell` double-borrow panic. A read that
/// wants a page another read is already fetching waits for that fetch instead
/// of issuing its own.
#[derive(Clone)]
pub struct PageCache(Rc<RefCell<PageCacheState>>);

struct PageCacheState {
    pages: ByteLru<Offset, Rc<[u8]>>,
    /// Page runs being fetched right now, so concurrent reads of the same
    /// pages make one request between them.
    in_flight: Vec<InFlight>,
}

/// The page run `[start, end)` that one read is fetching.
struct InFlight {
    start: Offset,
    end: Offset,
    done: Rc<FetchDone>,
}

impl PageCache {
    fn with_pages(pages: ByteLru<Offset, Rc<[u8]>>) -> Self {
        Self(Rc::new(RefCell::new(PageCacheState {
            pages,
            in_flight: Vec::new(),
        })))
    }

    /// A cache that evicts the least recently used pages once it holds more
    /// than `limit` bytes of them.
    pub fn bounded(limit: usize) -> Self {
        Self::with_pages(ByteLru::new(limit))
    }
}

/// Create an empty, unbounded [`PageCache`].
pub fn new_page_cache() -> PageCache {
    PageCache::with_pages(ByteLru::unbounded())
}

/// Set once the fetch it belongs to has finished, successfully or not, and
/// wakes everyone who waited on it.
#[derive(Default)]
struct FetchDone {
    done: Cell<bool>,
    wakers: RefCell<Vec<Waker>>,
}

struct WaitForFetch(Rc<FetchDone>);

impl Future for WaitForFetch {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.done.get() {
            return Poll::Ready(());
        }
        self.0.wakers.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

/// Unregisters a fetch and wakes its waiters when dropped, which covers the
/// fetch finishing, failing, and being cancelled alike: a waiter just looks in
/// the cache again and fetches whatever is still missing itself.
struct InFlightGuard {
    cache: PageCache,
    done: Rc<FetchDone>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.cache
            .0
            .borrow_mut()
            .in_flight
            .retain(|f| !Rc::ptr_eq(&f.done, &self.done));
        self.done.done.set(true);
        for waker in self.done.wakers.take() {
            waker.wake();
        }
    }
}

/// What [`CachingPageReader::ensure_pages`] decided to do about the first
/// missing page.
enum NextStep {
    Done,
    Wait(Rc<FetchDone>),
    Fetch {
        start: Offset,
        end: Offset,
        guard: InFlightGuard,
    },
}

/// A [`File`] wrapper that reads in 4 KiB pages and caches them, so repeated
/// or overlapping reads of the same region cost one underlying read.
pub struct CachingPageReader<F> {
    file: F,
    pages: PageCache,
    /// The least a fetch asks for, in bytes, so that nearby reads which follow
    /// find their pages already cached.
    readahead: usize,
}

impl<F: File> CachingPageReader<F> {
    /// Create a reader with its own private page cache.
    pub fn new(file: F) -> Self {
        Self::with_cache(file, new_page_cache())
    }

    /// Create a reader backed by an existing, shared page cache.
    pub fn with_cache(file: F, pages: PageCache) -> Self {
        Self {
            file,
            pages,
            readahead: 0,
        }
    }

    /// Have every fetch extend to at least `bytes`, past what the read asked
    /// for, as long as the pages it runs into are not already cached. Worth it
    /// for a file whose nearby bytes are likely to be read next.
    pub fn with_readahead(mut self, bytes: usize) -> Self {
        self.readahead = bytes;
        self
    }

    fn cached_page(&self, page_offset: Offset) -> Option<Rc<[u8]>> {
        self.pages.0.borrow_mut().pages.get(&page_offset).cloned()
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
        self.pages
            .0
            .borrow_mut()
            .pages
            .insert(page_offset, page.clone(), PAGE_SIZE);
        Ok(page)
    }

    /// Decide what to do about the first page in `[cursor, last_page]` that is
    /// not cached: nothing if there is none, wait if another read is already
    /// fetching it, or else claim a run of pages starting at it to fetch.
    fn next_step(&self, cursor: Offset, last_page: Offset) -> NextStep {
        let mut state = self.pages.0.borrow_mut();
        let in_flight_at = |state: &PageCacheState, page: Offset| {
            state
                .in_flight
                .iter()
                .find(|f| f.start <= page && page < f.end)
                .map(|f| f.done.clone())
        };
        let mut page = cursor;
        while state.pages.contains_key(&page) {
            if page >= last_page {
                return NextStep::Done;
            }
            page = page + PAGE_SIZE_U64;
        }
        if let Some(done) = in_flight_at(&state, page) {
            return NextStep::Wait(done);
        }
        // The run covers the rest of the read and then the readahead, but
        // stops short of any page that is cached or already on its way.
        let start = page;
        let readahead_end = start.0 + max(self.readahead, PAGE_SIZE) as u64;
        let limit = max(last_page.0 + PAGE_SIZE_U64, readahead_end);
        let mut end = start + PAGE_SIZE_U64;
        while end.0 < limit
            && !state.pages.contains_key(&end)
            && in_flight_at(&state, end).is_none()
        {
            end = end + PAGE_SIZE_U64;
        }
        let done = Rc::new(FetchDone::default());
        state.in_flight.push(InFlight {
            start,
            end,
            done: done.clone(),
        });
        NextStep::Fetch {
            start,
            end,
            guard: InFlightGuard {
                cache: self.pages.clone(),
                done,
            },
        }
    }

    /// Ensure every page spanning `[first_page, last_page]` (both page-aligned)
    /// is cached, fetching each contiguous run of missing pages in a single
    /// underlying read. For large sequential reads this collapses what would
    /// otherwise be one fetch per page into one request.
    async fn ensure_pages(
        &mut self,
        first_page: Offset,
        last_page: Offset,
    ) -> Result<(), FileSystemError> {
        // Pages this read fetched itself are never looked at again, so that a
        // run evicted as soon as it lands (a read bigger than the whole cache)
        // can't send it round in circles.
        let mut cursor = first_page;
        loop {
            match self.next_step(cursor, last_page) {
                NextStep::Done => return Ok(()),
                NextStep::Wait(done) => WaitForFetch(done).await,
                NextStep::Fetch { start, end, guard } => {
                    let span_len = usize::try_from(end.0 - start.0).unwrap();
                    let mut buf = vec![0u8; span_len];
                    let read_len = self.file.read_segment(start, &mut buf).await?;
                    buf.truncate(read_len);
                    {
                        let mut state = self.pages.0.borrow_mut();
                        let mut page_offset = start;
                        for chunk in buf.chunks(PAGE_SIZE) {
                            state.pages.insert(page_offset, Rc::from(chunk), PAGE_SIZE);
                            page_offset = page_offset + PAGE_SIZE_U64;
                        }
                    }
                    drop(guard);
                    // A short read is the end of the file: there is nothing
                    // further to fetch, and asking again would only repeat it.
                    if read_len < span_len || end > last_page {
                        return Ok(());
                    }
                    cursor = end;
                }
            }
        }
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

    /// A file over `len` bytes of `i % 251` that records every read it serves,
    /// and yields once before each so concurrent readers really overlap.
    struct CountingFile {
        data: Rc<Vec<u8>>,
        reads: Reads,
    }

    /// Every `(offset, len)` a [`CountingFile`] was asked for, in order.
    type Reads = Rc<RefCell<Vec<(u64, usize)>>>;

    fn counting_file(len: usize) -> (CountingFile, Reads) {
        let reads = Rc::new(RefCell::new(Vec::new()));
        let data = Rc::new((0..len).map(|i| (i % 251) as u8).collect());
        (
            CountingFile {
                data,
                reads: reads.clone(),
            },
            reads,
        )
    }

    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                return Poll::Ready(());
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    impl File for CountingFile {
        async fn read_all(&mut self) -> Result<Vec<u8>, FileSystemError> {
            Ok(self.data.to_vec())
        }

        async fn read_segment(
            &mut self,
            offset: Offset,
            dest: &mut [u8],
        ) -> Result<usize, FileSystemError> {
            self.reads.borrow_mut().push((offset.0, dest.len()));
            YieldOnce(false).await;
            let start = min(offset.0 as usize, self.data.len());
            let end = min(start + dest.len(), self.data.len());
            dest[..end - start].copy_from_slice(&self.data[start..end]);
            Ok(end - start)
        }
    }

    fn clone_file(file: &CountingFile) -> CountingFile {
        CountingFile {
            data: file.data.clone(),
            reads: file.reads.clone(),
        }
    }

    #[test]
    fn readahead_serves_the_next_reads_from_one_fetch() {
        let (file, reads) = counting_file(64 * PAGE_SIZE);
        let mut reader = CachingPageReader::new(file).with_readahead(8 * PAGE_SIZE);
        let mut buf = [0u8; 100];
        for offset in [10, 3 * PAGE_SIZE_U64, 7 * PAGE_SIZE_U64 + 5] {
            block_on(reader.read_segment(Offset(offset), &mut buf)).unwrap();
            assert_eq!(buf[0], (offset % 251) as u8);
        }
        assert_eq!(*reads.borrow(), [(0, 8 * PAGE_SIZE)]);
    }

    #[test]
    fn readahead_stops_at_cached_pages_and_at_eof() {
        let (file, reads) = counting_file(10 * PAGE_SIZE + 7);
        let pages = new_page_cache();
        let mut reader =
            CachingPageReader::with_cache(file, pages.clone()).with_readahead(4 * PAGE_SIZE);
        let mut buf = [0u8; 1];
        block_on(reader.read_segment(Offset(2 * PAGE_SIZE_U64), &mut buf)).unwrap();
        // Reading just before the cached run fetches only up to it.
        block_on(reader.read_segment(Offset(0), &mut buf)).unwrap();
        // Readahead past EOF is a short read, and the tail page is still served.
        let mut tail = [0u8; 8];
        let n = block_on(reader.read_segment(Offset(10 * PAGE_SIZE_U64), &mut tail)).unwrap();
        assert_eq!(n, 7);
        assert_eq!(
            *reads.borrow(),
            [
                (2 * PAGE_SIZE_U64, 4 * PAGE_SIZE),
                (0, 2 * PAGE_SIZE),
                (10 * PAGE_SIZE_U64, 4 * PAGE_SIZE),
            ]
        );
    }

    #[test]
    fn concurrent_reads_of_the_same_pages_share_one_fetch() {
        let (file, reads) = counting_file(16 * PAGE_SIZE);
        let pages = new_page_cache();
        let mut a = CachingPageReader::with_cache(clone_file(&file), pages.clone())
            .with_readahead(4 * PAGE_SIZE);
        let mut b =
            CachingPageReader::with_cache(file, pages.clone()).with_readahead(4 * PAGE_SIZE);
        let (mut buf_a, mut buf_b) = ([0u8; 10], [0u8; 10]);
        let (ra, rb) = block_on(futures::future::join(
            a.read_segment(Offset(5), &mut buf_a),
            b.read_segment(Offset(PAGE_SIZE_U64 + 5), &mut buf_b),
        ));
        assert_eq!((ra.unwrap(), rb.unwrap()), (10, 10));
        assert_eq!(buf_b[0], ((PAGE_SIZE + 5) % 251) as u8);
        assert_eq!(*reads.borrow(), [(0, 4 * PAGE_SIZE)]);
    }

    #[test]
    fn a_read_bigger_than_a_bounded_cache_still_completes() {
        let (file, reads) = counting_file(8 * PAGE_SIZE);
        let mut reader = CachingPageReader::with_cache(file, PageCache::bounded(2 * PAGE_SIZE));
        let mut buf = vec![0u8; 6 * PAGE_SIZE];
        let n = block_on(reader.read_segment(Offset(0), &mut buf)).unwrap();
        assert_eq!(n, buf.len());
        assert!(buf.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        // One fetch for the run, then one per page that had already been
        // evicted again by the time it was copied out.
        assert_eq!(reads.borrow()[0], (0, 6 * PAGE_SIZE));
    }
}
