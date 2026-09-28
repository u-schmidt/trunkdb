//! Data pages with room left, per collection, by their exact free bytes
//! (SPEC §75). In memory only: learned from writes, empty at open.
use std::collections::{BTreeSet, HashMap};

use crate::storage::PageId;

/// One collection's data pages that have room, ordered by free space so
/// an insert finds the fullest page that still fits in one lookup.
#[derive(Debug, Default, Clone)]
pub(crate) struct FreeSpace {
    /// `(free bytes, page)`, sorted by free bytes, then page.
    by_free: BTreeSet<(u16, PageId)>,
    /// Each page's current free bytes, to find its entry in `by_free`.
    free_of: HashMap<PageId, u16>,
}

impl FreeSpace {
    /// Records that `page` now has `free` bytes of room, replacing what
    /// was known about it before.
    pub(crate) fn set(&mut self, page: PageId, free: u16) {
        if let Some(old) = self.free_of.insert(page, free) {
            self.by_free.remove(&(old, page));
        }
        self.by_free.insert((free, page));
    }

    /// Forgets `page`: it was freed, or became a collection's current page.
    pub(crate) fn remove(&mut self, page: PageId) {
        if let Some(old) = self.free_of.remove(&page) {
            self.by_free.remove(&(old, page));
        }
    }

    /// Whether `page` is in the map.
    pub(crate) fn contains(&self, page: PageId) -> bool {
        self.free_of.contains_key(&page)
    }

    /// Every page and its free bytes, after checking the two collections
    /// agree — for tests that hold the map against the pages.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<(PageId, u16)> {
        let from_pages: BTreeSet<(u16, PageId)> = self
            .free_of
            .iter()
            .map(|(&page, &free)| (free, page))
            .collect();
        assert_eq!(
            from_pages, self.by_free,
            "the two halves of the map disagree"
        );
        self.by_free
            .iter()
            .map(|&(free, page)| (page, free))
            .collect()
    }

    /// The fullest page with at least `needed` bytes free, if any.
    pub(crate) fn find(&self, needed: u16) -> Option<PageId> {
        self.by_free
            .range((needed, 0)..)
            .next()
            .map(|&(_free, page)| page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_fullest_page_that_fits() {
        let mut map = FreeSpace::default();
        map.set(1, 3000);
        map.set(2, 500);
        map.set(3, 1200);

        assert_eq!(map.find(1000), Some(3)); // 1200 is the tightest fit
        assert_eq!(map.find(500), Some(2)); // exactly enough counts
        assert_eq!(map.find(3001), None); // no page has room
    }

    #[test]
    fn pages_with_the_same_free_space_are_both_kept() {
        let mut map = FreeSpace::default();
        map.set(7, 900);
        map.set(4, 900);

        assert_eq!(map.find(900), Some(4)); // equal space: lower page first
        map.remove(4);
        assert_eq!(map.find(900), Some(7));
    }

    #[test]
    fn a_new_value_replaces_the_old_one() {
        let mut map = FreeSpace::default();
        map.set(1, 3000);
        map.set(1, 200);

        assert_eq!(map.find(1000), None); // the old 3000 is gone
        assert_eq!(map.find(200), Some(1));
    }
}
