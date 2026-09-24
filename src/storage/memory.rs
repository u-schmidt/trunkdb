use super::{PageId, PageStore, USABLE_PAGE_SIZE};
use std::io;

/// Pages in memory, for building a whole database image before it goes
/// to a file in one batch — what `Database::compact` does (SPEC §41).
/// Page 0 is reserved, as in a file (the header, which `FileStore`
/// writes itself), so ids match the file the pages end up in.
///
/// Only grows: nothing a compaction runs frees a page, and a freed page
/// here would reach the file as a page nobody owns. So `free_page` is an
/// error rather than a free list nobody would use.
pub(crate) struct MemoryStore {
    /// Indexed by page id; `pages[0]` stands in for the header and is
    /// never read or written.
    pages: Vec<Vec<u8>>,
}

impl MemoryStore {
    pub(crate) fn new() -> Self {
        MemoryStore {
            pages: vec![Vec::new()],
        }
    }

    /// How many pages the image has, page 0 included — the file's page
    /// count once it's written.
    pub(crate) fn page_count(&self) -> u64 {
        self.pages.len() as u64
    }

    /// Every page but 0, with its id, in id order.
    pub(crate) fn into_pages(self) -> impl Iterator<Item = (PageId, Vec<u8>)> {
        self.pages
            .into_iter()
            .enumerate()
            .skip(1)
            .map(|(id, page)| (id as PageId, page))
    }

    fn check_id(&self, id: PageId) -> io::Result<usize> {
        match id {
            0 => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page 0 is the header, not a page to read or write",
            )),
            id if id >= self.page_count() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page {id} out of bounds (page_count = {})",
                    self.page_count()
                ),
            )),
            id => Ok(id as usize),
        }
    }
}

impl PageStore for MemoryStore {
    fn allocate_page(&mut self) -> io::Result<PageId> {
        self.pages.push(vec![0u8; USABLE_PAGE_SIZE]);
        Ok(self.page_count() - 1)
    }

    fn read_page(&self, id: PageId) -> io::Result<Vec<u8>> {
        Ok(self.pages[self.check_id(id)?].clone())
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Vec<u8>>> {
        if id >= self.page_count() {
            return Ok(None);
        }
        self.read_page(id).map(Some)
    }

    fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
        let index = self.check_id(id)?;
        if data.len() != USABLE_PAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page data must be exactly {USABLE_PAGE_SIZE} bytes, got {}",
                    data.len()
                ),
            ));
        }
        self.pages[index].copy_from_slice(data);
        Ok(())
    }

    fn free_page(&mut self, id: PageId) -> io::Result<()> {
        Err(io::Error::other(format!(
            "page {id}: a MemoryStore doesn't free pages (it builds an image that only grows)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_from_1_and_reads_back_what_was_written() {
        let mut store = MemoryStore::new();
        assert_eq!(store.try_read_page(1).unwrap(), None);
        assert_eq!(store.allocate_page().unwrap(), 1);
        assert_eq!(store.allocate_page().unwrap(), 2);
        store.write_page(2, &[7u8; USABLE_PAGE_SIZE]).unwrap();
        assert_eq!(store.read_page(1).unwrap(), vec![0u8; USABLE_PAGE_SIZE]);
        assert_eq!(
            store.try_read_page(2).unwrap(),
            Some(vec![7u8; USABLE_PAGE_SIZE])
        );
        assert_eq!(store.page_count(), 3);
        let pages: Vec<PageId> = store.into_pages().map(|(id, _)| id).collect();
        assert_eq!(pages, vec![1, 2]);
    }

    #[test]
    fn the_header_page_bounds_sizes_and_freeing_are_refused() {
        let mut store = MemoryStore::new();
        store.allocate_page().unwrap();
        assert!(store.read_page(0).is_err());
        assert!(store.write_page(0, &[0u8; USABLE_PAGE_SIZE]).is_err());
        assert!(store.read_page(2).is_err());
        assert!(store.write_page(1, &[0u8; 10]).is_err());
        assert!(store.free_page(1).is_err());
    }
}
