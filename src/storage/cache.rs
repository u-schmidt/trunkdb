use super::{Page, PageId};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};

/// Pages read from the file, kept in memory so the next read of one
/// costs a shared `Page` (SPEC §64) instead of a `pread`, a checksum and
/// an allocation (SPEC §50). Holds pages as they are on disk, checked:
/// `FileStore` puts a page
/// in after reading it or writing it back, and takes staged pages from
/// its dirty set before asking here, so a batch's changes never land in
/// the cache before they're in the file.
///
/// Eviction is CLOCK (second chance): a hand sweeps the slots; a page read
/// since the hand last passed is spared once, the first one that wasn't
/// goes. That approximates least-recently-used without keeping an order
/// on every read — a read only sets a flag. The flag is atomic, so a
/// read changes nothing else and needs only `&self`: readers share the
/// cache behind `Pages`' read lock, and only `put` and the others need
/// the write lock (SPEC §65, §79).
pub(crate) struct PageCache {
    /// At most this many pages; 0 keeps nothing.
    capacity: usize,
    slots: Vec<Slot>,
    /// Which slot holds which page.
    index: HashMap<PageId, usize>,
    /// The next slot the hand looks at.
    hand: usize,
    #[cfg(test)]
    pub(crate) hits: AtomicUsize,
    #[cfg(test)]
    pub(crate) misses: AtomicUsize,
}

struct Slot {
    id: PageId,
    page: Page,
    /// Read since the hand last passed.
    referenced: AtomicBool,
}

impl PageCache {
    pub(crate) fn new(capacity: usize) -> Self {
        PageCache {
            capacity,
            slots: Vec::new(),
            index: HashMap::new(),
            hand: 0,
            #[cfg(test)]
            hits: AtomicUsize::new(0),
            #[cfg(test)]
            misses: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Page `id`, if it's here: shared, not copied (SPEC §64), and
    /// through `&self`, so readers look pages up together (SPEC §65).
    pub(crate) fn get(&self, id: PageId) -> Option<Page> {
        match self.index.get(&id) {
            Some(&at) => {
                let slot = &self.slots[at];
                // Set only if it isn't: the pages every lookup reads, the
                // root first, would otherwise have their flag written by
                // every reader, each write taking the cache line from the
                // other cores.
                if !slot.referenced.load(Ordering::Relaxed) {
                    slot.referenced.store(true, Ordering::Relaxed);
                }
                #[cfg(test)]
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(slot.page.clone())
            }
            None => {
                #[cfg(test)]
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Puts page `id` in, replacing an older copy of it, or evicting
    /// another page if the cache is full. A reader still holding the
    /// page it replaces keeps that one.
    pub(crate) fn put(&mut self, id: PageId, page: Page) {
        if let Some(&at) = self.index.get(&id) {
            let slot = &mut self.slots[at];
            slot.page = page;
            *slot.referenced.get_mut() = true;
            return;
        }
        if self.capacity == 0 {
            return;
        }
        if self.slots.len() < self.capacity {
            self.index.insert(id, self.slots.len());
            self.slots.push(Slot {
                id,
                page,
                // Not read yet: the first to go if it never is.
                referenced: AtomicBool::new(false),
            });
            return;
        }
        loop {
            let at = self.hand;
            self.hand = (self.hand + 1) % self.slots.len();
            let slot = &mut self.slots[at];
            let referenced = slot.referenced.get_mut();
            if *referenced {
                *referenced = false;
                continue;
            }
            self.index.remove(&slot.id);
            self.index.insert(id, at);
            slot.id = id;
            slot.page = page;
            return;
        }
    }

    /// Forgets every page `keep` says no to.
    pub(crate) fn retain(&mut self, keep: impl Fn(PageId) -> bool) {
        if self.slots.iter().all(|slot| keep(slot.id)) {
            return;
        }
        self.slots.retain(|slot| keep(slot.id));
        self.index = self
            .slots
            .iter()
            .enumerate()
            .map(|(at, slot)| (slot.id, at))
            .collect();
        self.hand = 0;
    }

    /// Forgets everything; the capacity stays.
    pub(crate) fn clear(&mut self) {
        self.retain(|_| false);
    }

    /// Changes the capacity, forgetting pages over it.
    pub(crate) fn resize(&mut self, capacity: usize) {
        self.capacity = capacity;
        if self.slots.len() > capacity {
            self.slots.truncate(capacity);
            self.index = self
                .slots
                .iter()
                .enumerate()
                .map(|(at, slot)| (slot.id, at))
                .collect();
            self.hand = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8) -> Page {
        Page::from(vec![fill; 16])
    }

    fn has(cache: &PageCache, id: PageId) -> Option<u8> {
        cache.get(id).map(|page| page[0])
    }

    #[test]
    fn keeps_what_was_put_and_replaces_older_copies() {
        let mut cache = PageCache::new(3);
        assert_eq!(has(&cache, 1), None);
        cache.put(1, page(1));
        cache.put(2, page(2));
        assert_eq!(has(&cache, 1), Some(1));
        cache.put(1, page(9));
        assert_eq!(has(&cache, 1), Some(9));
        assert_eq!(cache.len(), 2);
    }

    /// Full, it evicts a page nobody read since the hand passed — the
    /// ones that were read get a second chance.
    #[test]
    fn a_full_cache_evicts_what_was_not_read() {
        let mut cache = PageCache::new(3);
        for id in 1..=3 {
            cache.put(id, page(id as u8));
        }
        assert_eq!(has(&cache, 1), Some(1));
        assert_eq!(has(&cache, 3), Some(3));
        cache.put(4, page(4));
        assert_eq!(cache.len(), 3);
        assert_eq!(has(&cache, 2), None, "the one not read goes");
        for id in [1, 3, 4] {
            assert_eq!(has(&cache, id), Some(id as u8));
        }
        // All read now: the hand clears them all, then takes the first.
        cache.put(5, page(5));
        assert_eq!(cache.len(), 3);
        assert_eq!(has(&cache, 5), Some(5));
        let kept = [1, 3, 4]
            .iter()
            .filter(|&&id| has(&cache, id).is_some())
            .count();
        assert_eq!(kept, 2);
    }

    /// A page put in again, as a checkpoint does with a page it wrote
    /// back, counts as used: the hand spares it once.
    #[test]
    fn a_page_replaced_is_spared_like_one_read() {
        let mut cache = PageCache::new(3);
        for id in 1..=3 {
            cache.put(id, page(id as u8));
        }
        cache.put(1, page(9));
        cache.put(4, page(4));
        assert_eq!(has(&cache, 1), Some(9));
        assert_eq!(has(&cache, 2), None, "the first one not used goes");
    }

    #[test]
    fn retain_clear_resize_and_nothing_at_capacity_zero() {
        let mut cache = PageCache::new(4);
        for id in 1..=4 {
            cache.put(id, page(id as u8));
        }
        cache.retain(|id| id % 2 == 0);
        assert_eq!(has(&cache, 1), None);
        assert_eq!(has(&cache, 2), Some(2));
        assert_eq!(has(&cache, 4), Some(4));
        // Still findable after the slots moved.
        cache.put(5, page(5));
        assert_eq!(has(&cache, 5), Some(5));
        cache.resize(1);
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.capacity(), 1);

        let mut none = PageCache::new(0);
        none.put(1, page(1));
        assert_eq!(has(&none, 1), None);
    }
}
