use crate::collection::Collection;
use crate::collection::Found;
use crate::document::Document;
use crate::index::key;
use crate::query::Filter;
use crate::select::{Answer, Selection};
use crate::storage::RecordLocation;

/// A streaming `find`: an iterator over a filter's matches, reading one
/// document at a time instead of collecting them all — see
/// `Collection::cursor` (SPEC §29.4).
///
/// A cursor shows one moment (SPEC §82): the collection as it was when
/// the cursor was created, whatever is written while it is open, also by
/// the code that iterates it. A document deleted meanwhile is still
/// handed out, one updated meanwhile as it was, one inserted meanwhile
/// not at all. For the documents as they are now, ask again: `get` by
/// the id each one carries.
///
/// It holds no lock, so writes go on while it is open. It does keep its
/// moment, as a `Snapshot` does: the pages changed since stay in memory
/// as they were, and `Database::compact` is refused, until the cursor is
/// dropped. Don't keep one around for longer than it is read.
///
/// At creation it takes where every candidate is (from an index range or
/// the primary index; no documents). Each `next` then reads one document
/// and checks it against the filter. Where the index entries decide the
/// filter (SPEC §100), they did so at creation: the rejected ones aren't
/// kept, and the matches a skip passes over are never read.
///
/// With a `sort`, nothing can stream: every match has to be read to know
/// which comes first. Such a cursor runs the whole `find` when it's
/// created and hands out its results — which, with a `limit` and an index
/// on the sort field, reads only as far as the limit (SPEC §34.2). It
/// keeps nothing.
#[must_use = "a cursor reads nothing until it's iterated"]
pub struct Cursor<T> {
    source: Source<T>,
}

/// A match decoded (SPEC §99.3): into a `Document` for the untyped path,
/// a `T` for the typed one, cut down to the selection if there is one
/// (SPEC §106).
pub(crate) type Convert<T> = fn(Found, Option<&Selection>) -> crate::Result<T>;

/// A candidate a streaming cursor still has to hand out or pass over.
pub(crate) enum Pending {
    /// To be read and checked.
    Unchecked(RecordLocation),
    /// Its index entry decided it matches (SPEC §100): it isn't checked,
    /// and a skip passes over it unread. With its key where the
    /// selection may be answered from it (§106.5).
    Matched(RecordLocation, Option<Vec<u8>>),
}

enum Source<T> {
    Streaming {
        /// The collection, as of the cursor's creation.
        documents: Collection<Document>,
        /// Where each candidate is, in that same commit: a location
        /// means nothing in another one, where its slot may hold a
        /// different document (SPEC §20.2).
        candidates: std::vec::IntoIter<Pending>,
        filter: Filter,
        /// The selection, and how the entries answer it (SPEC §106).
        selected: Option<Box<(Selection, Option<Answer>)>>,
        /// Matches still to pass over (`Filter::skip`, SPEC §72).
        skipping: usize,
        remaining: Option<usize>,
        convert: Convert<T>,
    },
    Collected(std::vec::IntoIter<T>),
}

impl<T> Cursor<T> {
    pub(crate) fn streaming(
        documents: Collection<Document>,
        candidates: Vec<Pending>,
        filter: Filter,
        select: Option<Selection>,
        answer: Option<Answer>,
        convert: Convert<T>,
    ) -> Self {
        let (skipping, remaining) = (filter.skip, filter.limit);
        let selected = select.map(|select| Box::new((select, answer)));
        Cursor {
            source: Source::Streaming {
                documents,
                candidates: candidates.into_iter(),
                filter,
                selected,
                skipping,
                remaining,
                convert,
            },
        }
    }

    pub(crate) fn collected(results: Vec<T>) -> Self {
        Cursor {
            source: Source::Collected(results.into_iter()),
        }
    }
}

impl<T> Iterator for Cursor<T> {
    /// A document, or the error reading it. After an error the cursor can
    /// go on with the next document. A document carries its id as `_id`,
    /// as `find` returns it (SPEC §59, §74).
    type Item = crate::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            Source::Collected(results) => results.next().map(Ok),
            Source::Streaming {
                documents,
                candidates,
                filter,
                selected,
                skipping,
                remaining,
                convert,
            } => {
                let select = selected.as_ref().map(|selected| &selected.0);
                let answer = selected.as_ref().and_then(|selected| selected.1.as_ref());
                if *remaining == Some(0) {
                    return None;
                }
                // Checked on the fields the filter names (SPEC §99.2).
                let names = filter.fields();
                let matcher = filter.matcher();
                for pending in candidates.by_ref() {
                    let (loc, decided, key) = match pending {
                        Pending::Unchecked(loc) => (loc, false, None),
                        Pending::Matched(loc, key) => (loc, true, key),
                    };
                    if decided && *skipping > 0 {
                        *skipping -= 1;
                        continue; // a match passed over unread
                    }
                    // Answered from its entry, unread (SPEC §106.5).
                    let answered = key
                        .zip(answer)
                        .and_then(|(key, answer)| Some((key::doc_id(&key), answer.entry(&key)?)));
                    if let Some((id, selected)) = answered {
                        if let Some(n) = remaining {
                            *n -= 1;
                        }
                        return Some(convert(Found::answered(id, selected), select));
                    }
                    let candidate = documents.stored_at(loc).and_then(|stored| {
                        if decided {
                            return Ok((stored, true));
                        }
                        let fields = stored.fields(names.as_deref())?;
                        Ok((stored, matcher.matches(&fields)?))
                    });
                    match candidate {
                        Err(e) => return Some(Err(e)),
                        Ok((stored, true)) => {
                            if *skipping > 0 {
                                *skipping -= 1;
                                continue;
                            }
                            if let Some(n) = remaining {
                                *n -= 1;
                            }
                            return Some(convert(Found::from(stored), select));
                        }
                        Ok(_) => continue, // a candidate that doesn't match
                    }
                }
                None
            }
        }
    }
}
