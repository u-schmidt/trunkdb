//! A regex in a filter (SPEC §169): `Filter::regex`, and `matches` in a
//! filter's text. Through the `regex` crate, whose matching takes time
//! linear in the text, so no pattern can make a query hang; it is behind
//! the `regex` feature, on by default, and not part of the API: only
//! this crate's own `Regex` and `RegexError` are.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

/// How large a pattern's compiled form may grow (SPEC §169.4): 1 MiB,
/// a tenth of the `regex` crate's own default. A pattern a person or a
/// program writes is far below it; `\w{100}`, a hundred of any
/// Unicode word character, isn't.
const SIZE_LIMIT: usize = 1 << 20;

/// How long a pattern's text may be, in bytes (SPEC §169.4).
pub(crate) const MAX_PATTERN: usize = 4096;

/// A regular expression for a filter (SPEC §169): compiled once, when it
/// is made, and shared by every filter it is in, and every thread one
/// runs on, as a cheap clone.
///
/// The syntax is the `regex` crate's: `time-?out`, `^Fix`, `\d{4}`,
/// `(?i)` to ignore case. It matches anywhere in a string, as `grep`
/// does; `^` and `$` anchor it to the string's start and end. Matching
/// takes time linear in the string, whatever the pattern: no
/// backreferences or look-around, which would allow patterns that take
/// exponential time.
///
/// ```
/// use trunkdb::query::{Filter, Regex};
///
/// let timeouts = Filter::new().regex("notes", Regex::new("time-?out").unwrap());
/// let fixes: Regex = "^Fix".parse().unwrap(); // read through an index on `title`
/// assert_eq!(fixes.as_str(), "^Fix");
/// assert!(Regex::new("(unclosed").is_err());
/// ```
#[derive(Clone)]
pub struct Regex(Arc<Inner>);

struct Inner {
    compiled: regex::Regex,
    /// The text every match begins with, where the pattern fixes one
    /// (SPEC §169.3): `Fix` for `^Fix(es)?`. An index reads it as the
    /// range `starts with` reads.
    prefix: Option<String>,
}

impl Regex {
    /// `pattern` compiled. A pattern that doesn't read, or that is over
    /// 4,096 bytes, or whose compiled form would pass 1 MiB, is a
    /// `RegexError` saying why.
    pub fn new(pattern: &str) -> Result<Regex, RegexError> {
        if pattern.len() > MAX_PATTERN {
            return Err(RegexError(format!(
                "the pattern is {} bytes long, over the limit of {MAX_PATTERN}",
                pattern.len()
            )));
        }
        let compiled = regex::RegexBuilder::new(pattern)
            .size_limit(SIZE_LIMIT)
            .build()
            .map_err(|e| RegexError(message(&e)))?;
        Ok(Regex(Arc::new(Inner {
            prefix: fixed_prefix(pattern),
            compiled,
        })))
    }

    /// The pattern, as it was written.
    pub fn as_str(&self) -> &str {
        self.0.compiled.as_str()
    }

    /// Whether the pattern matches somewhere in `text`.
    pub(crate) fn is_match(&self, text: &str) -> bool {
        self.0.compiled.is_match(text)
    }

    /// The text every match begins with, if the pattern fixes one.
    pub(crate) fn prefix(&self) -> Option<&str> {
        self.0.prefix.as_deref()
    }
}

/// The text every string the pattern matches begins with: the literals
/// right after a `^` that anchors to the start of the text, at the start
/// of a pattern that is one sequence. `None` for anything else:
/// `(?m)^`, which anchors to every line; an alternation, `^a|^b`; a
/// pattern ignoring case, whose letters are classes, not literals. What
/// comes after the literals, a repetition of the last letter included,
/// is the regex's to check: `^ab*` parses as `^`, `a`, `b*`.
fn fixed_prefix(pattern: &str) -> Option<String> {
    use regex_syntax::hir::{HirKind, Look};
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let HirKind::Concat(parts) = hir.kind() else {
        return None;
    };
    let mut parts = parts.iter();
    if !matches!(parts.next()?.kind(), HirKind::Look(Look::Start)) {
        return None;
    }
    let mut bytes = Vec::new();
    for part in parts {
        match part.kind() {
            HirKind::Literal(literal) => bytes.extend_from_slice(&literal.0),
            _ => break,
        }
    }
    // A literal is whole characters in a pattern of Unicode text; a
    // byte-oriented one, `(?-u)\xFF`, isn't text and fixes nothing.
    String::from_utf8(bytes).ok().filter(|p| !p.is_empty())
}

/// The `regex` crate's error, on one line: its syntax errors are several,
/// the pattern and a `^` under the place, then `error: …`.
fn message(error: &regex::Error) -> String {
    match error {
        regex::Error::CompiledTooBig(_) => {
            format!("the pattern is too big: compiled, it would pass {SIZE_LIMIT} bytes")
        }
        other => {
            let text = other.to_string();
            let line = text
                .lines()
                .find_map(|line| line.strip_prefix("error: "))
                .unwrap_or(&text);
            format!("not a regex: {line}")
        }
    }
}

impl FromStr for Regex {
    type Err = RegexError;

    fn from_str(pattern: &str) -> Result<Regex, RegexError> {
        Regex::new(pattern)
    }
}

/// The pattern, as it was written.
impl fmt::Display for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Regex").field(&self.as_str()).finish()
    }
}

/// Two regexes are equal if their patterns are, as written: `a+` and
/// `aa*` match the same strings and aren't.
impl PartialEq for Regex {
    fn eq(&self, other: &Regex) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Regex {}

/// Why a pattern isn't a `Regex` (SPEC §169.4), in a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexError(String);

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegexError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_prefix_is_found_where_every_match_begins_with_it() {
        let prefix = |p: &str| Regex::new(p).unwrap().prefix().map(str::to_string);
        assert_eq!(prefix("^Fix"), Some("Fix".into()));
        assert_eq!(prefix("^Fix(es)? login"), Some("Fix".into()));
        assert_eq!(prefix("^ab*"), Some("a".into()));
        assert_eq!(prefix("^ab?c"), Some("a".into()));
        assert_eq!(prefix(r"^a\.b.c"), Some("a.b".into()));
        assert_eq!(prefix(r"\AFix"), Some("Fix".into()));
        assert_eq!(prefix("^Grüße"), Some("Grüße".into()));
        // A class of one letter is that letter.
        assert_eq!(prefix("^[F]ix"), Some("Fix".into()));
        for none in [
            "Fix", "^", "^.ix", "(?i)^fix", "(?m)^Fix", "^a|^b", "^(Fix)", "^[Ff]ix", "x^Fix",
            "^*", "",
        ] {
            assert_eq!(prefix(none), None, "{none}");
        }
    }

    #[test]
    fn a_pattern_that_cant_be_one_says_why_in_a_line() {
        let refused = |p: &str| Regex::new(p).unwrap_err().to_string();
        assert!(
            refused("(unclosed").starts_with("not a regex: "),
            "{}",
            refused("(unclosed")
        );
        assert!(!refused("(unclosed").contains('\n'));
        assert!(
            refused(r"(a)\1").starts_with("not a regex: "),
            "no backreferences"
        );
        assert!(
            refused("(?=a)").starts_with("not a regex: "),
            "no look-around"
        );
        assert!(
            refused(r"\w{100}").contains("too big"),
            "{}",
            refused(r"\w{100}")
        );
        // Under the crate's own limit, 10 MiB: ours, not its, refuses it.
        assert!(::regex::Regex::new(r"\w{100}").is_ok());
        assert!(Regex::new(r"\w{10}").is_ok());
        assert!(refused(&"a".repeat(MAX_PATTERN + 1)).contains("over the limit"));
        assert!(Regex::new(&"a".repeat(MAX_PATTERN)).is_ok());
    }

    #[test]
    fn it_matches_anywhere_unless_anchored() {
        let re = |p: &str| Regex::new(p).unwrap();
        assert!(re("time-?out").is_match("a timeout here"));
        assert!(re("time-?out").is_match("time-out"));
        assert!(!re("^time").is_match("a timeout"));
        assert!(!re("out$").is_match("timeouts"));
        assert!(re("(?i)TIMEOUT").is_match("a timeout"));
        assert!(re("").is_match(""));
        assert_eq!(re("a+"), re("a+"));
        assert_ne!(re("a+"), re("aa*"));
        assert_eq!(format!("{:?}", re("a+")), r#"Regex("a+")"#);
    }
}
