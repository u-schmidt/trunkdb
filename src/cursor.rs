use crate::collection::Collection;
use crate::document::{DocId, Document};
use crate::query::Filter;

/// A streaming `find`: an iterator over a filter's matches, reading one
/// document at a time instead of collecting them all — see
/// `Collection::cursor` (SPEC §29.4).
///
/// Holds no lock between items. At creation it takes the ids of every
/// candidate (from an index range or the primary index; ids only, no
/// documents). Each `next` then looks one id up, reads that document
/// under a short read lock, and checks it against the filter. So writes
/// can happen while a cursor is open, with these effects:
/// - a document deleted since creation is skipped;
/// - a document updated since creation is checked in its current state,
///   and skipped if it no longer matches;
/// - a document inserted since creation isn't seen.
///
/// Every item is one document as it was at one moment — never half of a
/// batch — but different items may come from different moments.
///
/// With a `sort`, nothing can stream: every match has to be read to know
/// which comes first. Such a cursor runs the whole `find` when it's
/// created and hands out its results.
#[must_use = "a cursor reads nothing until it's iterated"]
pub struct Cursor<T> {
    source: Source<T>,
}

enum Source<T> {
    Streaming {
        documents: Collection<Document>,
        ids: std::vec::IntoIter<DocId>,
        filter: Filter,
        remaining: Option<usize>,
        /// `Document` to `T`: the identity for the untyped path, the
        /// serde bridge for the typed one.
        convert: fn(Document) -> crate::Result<T>,
    },
    Collected(std::vec::IntoIter<(DocId, T)>),
}

impl<T> Cursor<T> {
    pub(crate) fn streaming(
        documents: Collection<Document>,
        ids: Vec<DocId>,
        filter: Filter,
        convert: fn(Document) -> crate::Result<T>,
    ) -> Self {
        let remaining = filter.limit;
        Cursor {
            source: Source::Streaming {
                documents,
                ids: ids.into_iter(),
                filter,
                remaining,
                convert,
            },
        }
    }

    pub(crate) fn collected(results: Vec<(DocId, T)>) -> Self {
        Cursor {
            source: Source::Collected(results.into_iter()),
        }
    }
}

impl<T> Iterator for Cursor<T> {
    /// A document and its id, or the error reading it. After an error the
    /// cursor can go on with the next document.
    type Item = crate::Result<(DocId, T)>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            Source::Collected(results) => results.next().map(Ok),
            Source::Streaming {
                documents,
                ids,
                filter,
                remaining,
                convert,
            } => {
                if *remaining == Some(0) {
                    return None;
                }
                for id in ids.by_ref() {
                    match documents.get(&id) {
                        Err(e) => return Some(Err(e)),
                        Ok(None) => continue, // deleted since the cursor was created
                        Ok(Some(doc)) if filter.matches(&doc) => {
                            if let Some(n) = remaining {
                                *n -= 1;
                            }
                            return Some(convert(doc).map(|t| (id, t)));
                        }
                        Ok(Some(_)) => continue, // a candidate that doesn't match (any more)
                    }
                }
                None
            }
        }
    }
}
