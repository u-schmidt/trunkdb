use crate::document::DocId;

/// A pluggable seam for id generation — costs nothing today, but means a
/// caller-supplied id scheme is a drop-in swap later, not a redesign.
pub trait IdGenerator {
    fn generate(&self) -> DocId;
}

/// The default: UUIDv7. Chosen over UUIDv4 deliberately — v7 embeds a
/// timestamp prefix, so ids stay roughly time-ordered, which matters once
/// `LinearIndex` is replaced by a real B-tree (random ids scatter inserts
/// and cause more page splits).
#[derive(Debug, Default, Clone, Copy)]
pub struct UuidV7Generator;

impl IdGenerator for UuidV7Generator {
    fn generate(&self) -> DocId {
        DocId(*uuid::Uuid::now_v7().as_bytes())
    }
}
