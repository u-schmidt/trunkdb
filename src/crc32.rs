//! CRC-32C (Castagnoli, the one iSCSI, ext4 and RocksDB use), for WAL
//! records (SPEC §19) and every page in the file (SPEC §40).
//!
//! Pages are checked on every read, and a scan reads a data page once
//! per document on it, so this has to be fast. Castagnoli rather than the
//! zlib CRC-32 because x86-64 (SSE 4.2) and ARM64 compute it in hardware,
//! eight bytes per instruction; elsewhere a table-driven version takes
//! eight bytes per step ("slicing-by-8"). Its tables are built at compile
//! time, so there's nothing to initialize and no dependency.

/// The Castagnoli polynomial, bit-reversed (the CRC runs least
/// significant bit first).
const POLYNOMIAL: u32 = 0x82F6_3B78;

/// `TABLES[0][b]`: the CRC of byte `b` alone. `TABLES[k][b]`: the same
/// byte followed by `k` zero bytes — what lets one step take eight bytes
/// at once.
static TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (POLYNOMIAL & (crc & 1).wrapping_neg());
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut byte = 0;
    while byte < 256 {
        let mut k = 1;
        while k < 8 {
            let previous = tables[k - 1][byte];
            tables[k][byte] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            k += 1;
        }
        byte += 1;
    }
    tables
}

/// A CRC over several pieces, as if they were one: `new`, `update` per
/// piece, `finish`.
pub(crate) struct Crc32(u32);

impl Crc32 {
    pub(crate) fn new() -> Self {
        Crc32(!0)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.0 = update(self.0, bytes);
    }

    pub(crate) fn finish(&self) -> u32 {
        !self.0
    }
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

#[cfg(target_arch = "x86_64")]
fn update(crc: u32, bytes: &[u8]) -> u32 {
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU has SSE 4.2, just checked.
        unsafe { update_x86_64(crc, bytes) }
    } else {
        update_table(crc, bytes)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
fn update_x86_64(crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let (chunks, rest) = bytes.as_chunks::<8>();
    let mut crc = crc as u64;
    for chunk in chunks {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(*chunk));
    }
    let mut crc = crc as u32;
    for &byte in rest {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
}

#[cfg(target_arch = "aarch64")]
fn update(crc: u32, bytes: &[u8]) -> u32 {
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the CPU has the CRC instructions, just checked.
        unsafe { update_aarch64(crc, bytes) }
    } else {
        update_table(crc, bytes)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
fn update_aarch64(crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let (chunks, rest) = bytes.as_chunks::<8>();
    let mut crc = crc;
    for chunk in chunks {
        crc = __crc32cd(crc, u64::from_le_bytes(*chunk));
    }
    for &byte in rest {
        crc = __crc32cb(crc, byte);
    }
    crc
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn update(crc: u32, bytes: &[u8]) -> u32 {
    update_table(crc, bytes)
}

fn update_table(mut crc: u32, bytes: &[u8]) -> u32 {
    let t = &TABLES;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
        let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        crc = t[7][(low & 0xFF) as usize]
            ^ t[6][((low >> 8) & 0xFF) as usize]
            ^ t[5][((low >> 16) & 0xFF) as usize]
            ^ t[4][(low >> 24) as usize]
            ^ t[3][(high & 0xFF) as usize]
            ^ t[2][((high >> 8) & 0xFF) as usize]
            ^ t[1][((high >> 16) & 0xFF) as usize]
            ^ t[0][(high >> 24) as usize];
    }
    for &byte in rest {
        crc = (crc >> 8) ^ t[0][((crc ^ byte as u32) & 0xFF) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition, one bit at a time — what both the tables and the
    /// hardware must agree with.
    fn bitwise(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (POLYNOMIAL & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }

    #[test]
    fn matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xE306_9283);
        assert_eq!(crc32(b""), 0);
    }

    /// Every length from 0 to past two 8-byte steps, so both the 8-byte
    /// loop and the leftover bytes are covered — for whichever `update`
    /// this CPU uses, and for the tables on every CPU.
    #[test]
    fn matches_the_bitwise_definition() {
        let bytes: Vec<u8> = (0..300u32).map(|i| (i * 131 + 7) as u8).collect();
        for len in 0..=bytes.len() {
            let expected = bitwise(&bytes[..len]);
            assert_eq!(crc32(&bytes[..len]), expected, "{len} bytes");
            assert_eq!(!update_table(!0, &bytes[..len]), expected, "{len} bytes");
        }
    }

    #[test]
    fn pieces_give_the_same_crc_as_the_whole() {
        let bytes: Vec<u8> = (0..100u8).collect();
        for split in 0..=bytes.len() {
            let mut crc = Crc32::new();
            crc.update(&bytes[..split]);
            crc.update(&bytes[split..]);
            assert_eq!(crc.finish(), crc32(&bytes), "split at {split}");
        }
    }
}
