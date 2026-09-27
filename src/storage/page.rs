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
#[derive(Clone, PartialEq, Eq)]
pub struct Page(Arc<[u8]>);

impl Page {
    /// The bytes, to change: copied first if the page is shared.
    pub fn make_mut(&mut self) -> &mut [u8] {
        Arc::make_mut(&mut self.0)
    }

    /// Whether `self` and `other` are the same bytes in memory, not just
    /// equal ones.
    #[cfg(test)]
    pub(crate) fn shares(&self, other: &Page) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for Page {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for Page {
    fn from(bytes: &[u8]) -> Self {
        Page(bytes.into())
    }
}

impl From<Vec<u8>> for Page {
    fn from(bytes: Vec<u8>) -> Self {
        Page(bytes.into())
    }
}

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
}
