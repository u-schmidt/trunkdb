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
}
