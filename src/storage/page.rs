use std::sync::Arc;

/// A page's bytes, shared (SPEC §64): the page cache hands out the page it
/// holds instead of a copy, so a read costs an atomic increment, not an
/// allocation and 8 KB copied while holding the cache's lock (§58.3).
/// Cloning shares too.
///
/// A page is changed through `make_mut`, which copies it first if anyone
/// else still holds it: copy on write. So whoever holds a page sees it as
/// it was when they got it, as with the copy a read used to make, and a
/// page the cache holds is never changed under it.
#[derive(Clone)]
pub struct Page {
    bytes: Arc<[u8]>,
    /// Whether the bytes were found a valid slotted page (SPEC §66): set
    /// when a page goes into the cache, copied with every clone the cache
    /// hands out, so `SlottedPage::from_bytes` checks a cached page once,
    /// not on every read. In the handle, not with the shared bytes, so
    /// reading it touches no memory other cores write.
    layout_checked: bool,
}

impl Page {
    /// The bytes, to change: copied first if the page is shared. The
    /// layout counts as unchecked afterwards (SPEC §66).
    pub fn make_mut(&mut self) -> &mut [u8] {
        self.layout_checked = false;
        Arc::make_mut(&mut self.bytes)
    }

    /// Whether the bytes were found a valid slotted page since they last
    /// changed (SPEC §66).
    pub(crate) fn layout_checked(&self) -> bool {
        self.layout_checked
    }

    /// The page, marked as a valid slotted page if it is one (SPEC §66):
    /// what goes into the cache, or among the committed pages. Checked
    /// only if it isn't marked yet.
    pub(crate) fn checked(mut self) -> Page {
        if !self.layout_checked {
            self.layout_checked = super::SlottedPage::check_layout(&self).is_ok();
        }
        self
    }

    /// The page marked checked without being checked, as no code but a
    /// test may do: to see that a marked page isn't checked again.
    #[cfg(test)]
    pub(crate) fn marked_unchecked(self) -> Page {
        Page {
            layout_checked: true,
            ..self
        }
    }

    /// Whether `self` and `other` are the same bytes in memory, not just
    /// equal ones.
    #[cfg(test)]
    pub(crate) fn shares(&self, other: &Page) -> bool {
        Arc::ptr_eq(&self.bytes, &other.bytes)
    }
}

impl std::ops::Deref for Page {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl From<&[u8]> for Page {
    fn from(bytes: &[u8]) -> Self {
        Page {
            bytes: bytes.into(),
            layout_checked: false,
        }
    }
}

impl From<Vec<u8>> for Page {
    fn from(bytes: Vec<u8>) -> Self {
        Page {
            bytes: bytes.into(),
            layout_checked: false,
        }
    }
}

/// Equal bytes; whether either was checked doesn't count.
impl PartialEq for Page {
    fn eq(&self, other: &Page) -> bool {
        **self == **other
    }
}

impl Eq for Page {}

/// So tests can compare a page with the bytes they expect.
impl PartialEq<Vec<u8>> for Page {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == **other
    }
}

/// The length and the first bytes, not all 8 KB.
impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = self.len().min(16);
        write!(f, "Page({} bytes: {:?}", self.len(), &self[..shown])?;
        if self.len() > shown {
            f.write_str(" ...")?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_until_one_side_changes() {
        let a = Page::from(vec![1u8; 32]);
        let mut b = a.clone();
        assert!(b.shares(&a));
        b.make_mut()[0] = 9;
        assert!(!b.shares(&a), "copied on the first write");
        assert_eq!((a[0], b[0]), (1, 9));

        // Held by no one else, a page is changed where it is.
        let before = b.as_ptr();
        b.make_mut()[1] = 8;
        assert_eq!(b.as_ptr(), before);
        assert_eq!(b[..3], [9, 8, 1]);
    }

    /// SPEC §66: the mark goes with every clone, and a change, copied or
    /// in place, takes it away from the page changed.
    #[test]
    fn a_change_takes_the_layout_mark_away() {
        let a = Page::from(vec![1u8; 32]).marked_unchecked();
        let mut b = a.clone();
        assert!(b.layout_checked());
        b.make_mut()[0] = 9;
        assert!(!b.layout_checked(), "copied");
        assert!(a.layout_checked(), "the original didn't change");

        let mut only = a;
        only.make_mut()[0] = 8;
        assert!(!only.layout_checked(), "changed in place");
    }
}
