//! Hooks for the fuzz targets in `fuzz/` (SPEC §55), behind the `fuzzing`
//! feature and hidden from the docs: not for applications.
//!
//! Every page ends with a checksum and every WAL record carries one, so
//! bytes a fuzzer makes up would almost never get past them, and the
//! fuzzer would only ever test the checksums. These functions seal the
//! fuzzer's bytes with valid ones, so what's behind the checksums gets
//! tested: everything that decodes a page, a cell or a record.

use crate::storage::{PAGE_SIZE, PageId, USABLE_PAGE_SIZE, checksum};

/// A database file made of `pages`, cut into `USABLE_PAGE_SIZE` pieces,
/// each followed by its checksum. A last piece that is shorter is padded
/// with zeros.
pub fn seal(pages: &[u8]) -> Vec<u8> {
    let mut file = Vec::with_capacity(pages.len().div_ceil(USABLE_PAGE_SIZE) * PAGE_SIZE);
    for (id, piece) in pages.chunks(USABLE_PAGE_SIZE).enumerate() {
        let mut page = piece.to_vec();
        page.resize(USABLE_PAGE_SIZE, 0);
        file.extend_from_slice(&page);
        file.extend_from_slice(&checksum(id as PageId, &page));
    }
    file
}

/// A database file's pages without their checksums: `seal` undone.
pub fn unseal(file: &[u8]) -> Vec<u8> {
    file.chunks(PAGE_SIZE)
        .flat_map(|page| &page[..USABLE_PAGE_SIZE.min(page.len())])
        .copied()
        .collect()
}

/// A WAL file holding one valid record of `pages`, each `(id, bytes)`
/// with `USABLE_PAGE_SIZE` bytes — what a commit of them would log.
pub fn wal(pages: &[(PageId, Vec<u8>)]) -> Vec<u8> {
    let pages: Vec<(PageId, &[u8])> = pages.iter().map(|(id, page)| (*id, &page[..])).collect();
    let mut file = crate::durability::encode_header().to_vec();
    file.extend_from_slice(&crate::durability::encode_record(&pages));
    file
}
