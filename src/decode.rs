//! Reading bytes that came from the file or the WAL (SPEC §55). A length
//! or count in them may be wrong: a checksum proves the bytes are the ones
//! written, not that what wrote them was right. So running out of bytes is
//! an `InvalidData` error here, never a panic in the host application,
//! and a count never sizes an allocation before the bytes behind it are
//! known to be there.

use std::io;

/// An `InvalidData` error: `what` is damaged.
pub(crate) fn corrupt(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what} — file may be corrupt"),
    )
}

/// The first `n` bytes of `bytes`, which moves past them; an error naming
/// `what` if there are fewer.
pub(crate) fn take<'a>(bytes: &mut &'a [u8], n: usize, what: &str) -> io::Result<&'a [u8]> {
    if bytes.len() < n {
        return Err(corrupt(format_args!(
            "{what} needs {n} bytes, {} left",
            bytes.len()
        )));
    }
    let (taken, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(taken)
}

/// `take`, as an array.
pub(crate) fn take_array<const N: usize>(bytes: &mut &[u8], what: &str) -> io::Result<[u8; N]> {
    Ok(take(bytes, N, what)?.try_into().expect("take gave N bytes"))
}

pub(crate) fn take_u8(bytes: &mut &[u8], what: &str) -> io::Result<u8> {
    Ok(take_array::<1>(bytes, what)?[0])
}

pub(crate) fn take_u32(bytes: &mut &[u8], what: &str) -> io::Result<u32> {
    take_array(bytes, what).map(u32::from_le_bytes)
}

pub(crate) fn take_u64(bytes: &mut &[u8], what: &str) -> io::Result<u64> {
    take_array(bytes, what).map(u64::from_le_bytes)
}

/// An unsigned LEB128 varint: seven bits a byte, lowest first, the high
/// bit set on every byte but the last. At most ten bytes, the tenth
/// holding only the top bit of a `u64`.
pub(crate) fn take_varint(bytes: &mut &[u8], what: &str) -> io::Result<u64> {
    // Most varints are one byte.
    if let Some((&byte, rest)) = bytes.split_first()
        && byte < 0x80
    {
        *bytes = rest;
        return Ok(u64::from(byte));
    }
    let mut value = 0u64;
    for (n, &byte) in bytes.iter().enumerate().take(10) {
        let bits = u64::from(byte & 0x7F);
        if n == 9 && byte > 1 {
            return Err(corrupt(format_args!("{what} is a varint past 64 bits")));
        }
        value |= bits << (7 * n);
        if byte & 0x80 == 0 {
            *bytes = &bytes[n + 1..];
            return Ok(value);
        }
    }
    Err(corrupt(format_args!("{what} is a varint that doesn't end")))
}

/// `value` as `take_varint` reads it.
pub(crate) fn put_varint(mut value: u64, buffer: &mut Vec<u8>) {
    while value >= 0x80 {
        buffer.push(value as u8 | 0x80);
        value >>= 7;
    }
    buffer.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_moves_past_what_it_takes_and_refuses_what_is_not_there() {
        let mut bytes: &[u8] = &[1, 0, 0, 0, 9, 8];
        assert_eq!(take_u32(&mut bytes, "a count").unwrap(), 1);
        assert_eq!(take(&mut bytes, 1, "a byte").unwrap(), [9]);
        let err = take(&mut bytes, 2, "two bytes").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("two bytes needs 2 bytes, 1 left"),
            "{err}"
        );
        assert_eq!(bytes, [8], "a failed take moves nothing");
    }

    #[test]
    fn varints_read_back_and_damage_is_refused() {
        for value in [
            0,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut buffer = Vec::new();
            put_varint(value, &mut buffer);
            let mut bytes = &buffer[..];
            assert_eq!(take_varint(&mut bytes, "v").unwrap(), value);
            assert!(bytes.is_empty());
        }
        assert_eq!(
            [1usize, 127, 128, 16_383, 16_384].map(|v| {
                let mut b = Vec::new();
                put_varint(v as u64, &mut b);
                b.len()
            }),
            [1, 1, 2, 2, 3]
        );
        let mut unended: &[u8] = &[0x80, 0x80];
        assert!(take_varint(&mut unended, "v").is_err());
        assert_eq!(unended.len(), 2, "a failed take moves nothing");
        let mut too_wide: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
        assert!(take_varint(&mut too_wide, "v").is_err());
        let mut eleven: &[u8] = &[0x80; 11];
        assert!(take_varint(&mut eleven, "v").is_err());
    }
}
