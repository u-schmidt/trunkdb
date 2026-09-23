use super::{Index, KeyRange};
use crate::storage::{PageStore, RecordLocation};
use std::collections::BTreeMap;

/// A plain `BTreeMap` — the non-durable stand-in for `BTreeIndex`.
/// Ignores the `store` parameter completely, since it never touches disk;
/// useful for tests that don't want real file I/O, and as the reference
/// `BTreeIndex` is checked against.
#[derive(Default)]
pub struct InMemoryIndex {
    entries: BTreeMap<Vec<u8>, RecordLocation>,
}

impl Index for InMemoryIndex {
    fn insert(
        &mut self,
        _store: &mut dyn PageStore,
        key: &[u8],
        loc: RecordLocation,
    ) -> std::io::Result<()> {
        self.entries.insert(key.to_vec(), loc);
        Ok(())
    }

    fn remove(&mut self, _store: &mut dyn PageStore, key: &[u8]) -> std::io::Result<()> {
        self.entries.remove(key);
        Ok(())
    }

    fn lookup(
        &self,
        _store: &dyn PageStore,
        key: &[u8],
    ) -> std::io::Result<Option<RecordLocation>> {
        Ok(self.entries.get(key).copied())
    }

    fn range(
        &self,
        _store: &dyn PageStore,
        range: &KeyRange,
    ) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>> {
        Ok(self
            .entries
            .range(range.start.clone()..)
            .take_while(|(key, _)| range.contains(key))
            .map(|(key, loc)| (key.clone(), *loc))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PageId;

    // A PageStore that panics on any real call — since InMemoryIndex never
    // touches `store` at all, this doubles as proof of that: if it ever
    // did, this test would panic instead of passing.
    struct NoStore;
    impl PageStore for NoStore {
        fn allocate_page(&mut self) -> std::io::Result<PageId> {
            unimplemented!()
        }
        fn read_page(&self, _id: PageId) -> std::io::Result<Vec<u8>> {
            unimplemented!()
        }
        fn try_read_page(&self, _id: PageId) -> std::io::Result<Option<Vec<u8>>> {
            unimplemented!()
        }
        fn write_page(&mut self, _id: PageId, _data: &[u8]) -> std::io::Result<()> {
            unimplemented!()
        }
        fn free_page(&mut self, _id: PageId) -> std::io::Result<()> {
            unimplemented!()
        }
    }

    #[test]
    fn insert_lookup_remove_roundtrip() {
        let mut idx = InMemoryIndex::default();
        let mut store = NoStore;
        let key = b"forty-two";
        let loc = RecordLocation { page: 1, slot: 0 };

        idx.insert(&mut store, key, loc).unwrap();
        assert_eq!(idx.lookup(&store, key).unwrap(), Some(loc));
        assert_eq!(idx.scan(&store).unwrap(), vec![(key.to_vec(), loc)]);

        idx.remove(&mut store, key).unwrap();
        assert_eq!(idx.lookup(&store, key).unwrap(), None);
    }
}
