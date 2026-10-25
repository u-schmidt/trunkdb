//! `Release`, a trunkdb release by its version's numbers (SPEC §125):
//! what the header records as the release that created a file and the
//! one that wrote it last, and `RELEASE`, this build's.

use std::fmt;

/// A trunkdb release: its version's major, minor and patch numbers, as
/// `Cargo.toml` has them, without a pre-release part (SPEC §125.5).
/// Ordered as releases are: `0.9.0` before `0.10.0`.
///
/// ```
/// let release = trunkdb::RELEASE;
/// assert_eq!(release.to_string(), env!("CARGO_PKG_VERSION"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Release {
    /// The major version: `0` before 1.0.
    pub major: u16,
    /// The minor version: `24` in `0.24.0`.
    pub minor: u16,
    /// The patch version: `1` in `0.24.1`.
    pub patch: u16,
}

/// The release this build of trunkdb is, from `Cargo.toml`. A build
/// between releases says the last one: `Cargo.toml` names it until the
/// next bump (SPEC §125.5).
pub const RELEASE: Release = Release {
    major: parse(env!("CARGO_PKG_VERSION_MAJOR")),
    minor: parse(env!("CARGO_PKG_VERSION_MINOR")),
    patch: parse(env!("CARGO_PKG_VERSION_PATCH")),
};

impl Release {
    /// How the header holds it: three little-endian `u16`s.
    pub(crate) fn to_bytes(self) -> [u8; 6] {
        let mut bytes = [0u8; 6];
        bytes[0..2].copy_from_slice(&self.major.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.minor.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.patch.to_le_bytes());
        bytes
    }

    /// From the header's six bytes: `None` for all zeros, unknown — a
    /// file from before §125, or one an older build wrote since.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let part = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let release = Release {
            major: part(0),
            minor: part(2),
            patch: part(4),
        };
        (release != Release::UNKNOWN).then_some(release)
    }

    /// `0.0.0`, which no release of trunkdb ever was: how the header says
    /// unknown.
    pub(crate) const UNKNOWN: Release = Release {
        major: 0,
        minor: 0,
        patch: 0,
    };

    /// A release by its numbers, for tests that stand in for another
    /// build.
    #[cfg(test)]
    pub(crate) const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Release {
            major,
            minor,
            patch,
        }
    }
}

/// `0.24.0`.
impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A version part from `Cargo.toml`, at compile time: Cargo has already
/// checked it's a number; one past `u16` fails the build.
const fn parse(digits: &str) -> u16 {
    let bytes = digits.as_bytes();
    assert!(!bytes.is_empty(), "an empty version part");
    let mut value: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        assert!(
            bytes[i].is_ascii_digit(),
            "a version part that isn't a number"
        );
        value = value * 10 + (bytes[i] - b'0') as u32;
        assert!(value <= u16::MAX as u32, "a version part past u16");
        i += 1;
    }
    value as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_build_is_the_version_cargo_has() {
        assert_eq!(RELEASE.to_string(), env!("CARGO_PKG_VERSION"));
        assert_ne!(RELEASE, Release::UNKNOWN);
    }

    #[test]
    fn bytes_round_trip_and_zeros_are_unknown() {
        let release = Release::new(1, 300, 65_535);
        assert_eq!(
            release.to_bytes(),
            [1, 0, 0x2c, 0x01, 0xff, 0xff],
            "little-endian, major first"
        );
        assert_eq!(Release::from_bytes(&release.to_bytes()), Some(release));
        assert_eq!(Release::from_bytes(&[0; 6]), None);
        assert_eq!(
            Release::from_bytes(&Release::new(0, 0, 1).to_bytes()),
            Some(Release::new(0, 0, 1))
        );
    }

    #[test]
    fn releases_order_by_number_not_text() {
        assert!(Release::new(0, 9, 0) < Release::new(0, 10, 0));
        assert!(Release::new(0, 24, 9) < Release::new(0, 25, 0));
        assert!(Release::new(0, 99, 0) < Release::new(1, 0, 0));
        assert_eq!(Release::new(0, 24, 0).to_string(), "0.24.0");
    }

    #[test]
    fn parse_reads_digits() {
        assert_eq!(parse("0"), 0);
        assert_eq!(parse("24"), 24);
        assert_eq!(parse("65535"), 65_535);
    }
}
