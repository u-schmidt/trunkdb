//! Filters and conditions as text (SPEC §111): `FromStr` reads the
//! SQL-like language into a `Filter` or `Condition`, `Display` writes one
//! back. The text says what the builders say, no more: it compiles to
//! the same filter, which the planner sees as it sees any other.

use crate::datetime::DateTime;
use crate::document::{DocId, Document};
use crate::query::{Condition, Filter, Op, Sort, SortOrder};
use std::fmt::{self, Formatter, Write};
use std::str::FromStr;

/// Words that are never a bare field name: written in backticks, a field
/// may have any of them as its name (`` `order` ``). The other keywords
/// (`is`, `has`, `contains`, `by`, `asc`, `desc`, and `exists`, `size`,
/// `id`, `date` before a `(`) only mean something where a field can't
/// be, so `size > 3` compares a field named `size`.
const RESERVED: [&str; 11] = [
    "and", "or", "not", "true", "false", "null", "select", "where", "order", "limit", "offset",
];

/// Text that isn't a filter (SPEC §111.5): what's wrong, and where.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at line {line} column {column}")]
pub struct ParseFilterError {
    message: String,
    offset: usize,
    line: usize,
    column: usize,
}

impl ParseFilterError {
    fn new(text: &str, offset: usize, message: impl Into<String>) -> Self {
        let before = &text[..offset];
        let line_start = before.rfind('\n').map_or(0, |at| at + 1);
        ParseFilterError {
            message: message.into(),
            offset,
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
        }
    }

    /// Where in the text, in bytes from its start: for a tool to mark it.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// A filter from its text (SPEC §111):
///
/// ```
/// use trunkdb::query::Filter;
///
/// let newest_complete: Filter = r#"
///     status = "Complete" and seen >= 10
///     order by started_at desc
///     limit 1
/// "#.parse()?;
/// # Ok::<(), trunkdb::query::ParseFilterError>(())
/// ```
impl FromStr for Filter {
    type Err = ParseFilterError;

    fn from_str(text: &str) -> Result<Filter, ParseFilterError> {
        let mut parser = Parser { text, at: 0 };
        let filter = parser.filter()?;
        parser.end()?;
        Ok(filter)
    }
}

/// A condition from its text (SPEC §111): what follows `where`.
impl FromStr for Condition {
    type Err = ParseFilterError;

    fn from_str(text: &str) -> Result<Condition, ParseFilterError> {
        let mut parser = Parser { text, at: 0 };
        let condition = parser.condition()?;
        parser.end()?;
        Ok(condition)
    }
}

type Parsed<T> = Result<T, ParseFilterError>;

/// A recursive descent over the text, without tokens: what a word or a
/// `[` means depends on where it is.
struct Parser<'t> {
    text: &'t str,
    /// The byte the next read starts at.
    at: usize,
}

impl<'t> Parser<'t> {
    fn rest(&self) -> &'t str {
        &self.text[self.at..]
    }

    fn skip_space(&mut self) {
        self.at = self.text.len() - self.rest().trim_start().len();
    }

    fn error<T>(&self, message: impl Into<String>) -> Parsed<T> {
        self.error_at(self.at, message)
    }

    fn error_at<T>(&self, at: usize, message: impl Into<String>) -> Parsed<T> {
        Err(ParseFilterError::new(self.text, at, message))
    }

    /// An error here, past any space: `expected {what}, found` what's
    /// next — a word, a character, or the end.
    fn expected<T>(&mut self, what: &str) -> Parsed<T> {
        self.skip_space();
        let word = self.peek_word();
        let found = match self.rest().chars().next() {
            None => "the end".to_string(),
            Some(_) if !word.is_empty() => format!("`{word}`"),
            Some(c) => format!("`{c}`"),
        };
        self.error(format!("expected {what}, found {found}"))
    }

    /// Past `symbol`, after any space, if it's next.
    fn eat(&mut self, symbol: &str) -> bool {
        self.skip_space();
        let next = self.rest().starts_with(symbol);
        if next {
            self.at += symbol.len();
        }
        next
    }

    fn expect(&mut self, symbol: &str) -> Parsed<()> {
        match self.eat(symbol) {
            true => Ok(()),
            false => self.expected(&format!("`{symbol}`")),
        }
    }

    /// The word starting here, not read past: empty if none does.
    fn peek_word(&self) -> &'t str {
        let rest = self.rest();
        if !rest.starts_with(starts_word) {
            return "";
        }
        let end = rest.find(|c| !in_word(c)).unwrap_or(rest.len());
        &rest[..end]
    }

    /// Past `keyword`, in any case, if it's the next word.
    fn keyword(&mut self, keyword: &str) -> bool {
        self.skip_space();
        let word = self.peek_word();
        let next = word.eq_ignore_ascii_case(keyword);
        if next {
            self.at += word.len();
        }
        next
    }

    fn expect_keyword(&mut self, keyword: &str) -> Parsed<()> {
        match self.keyword(keyword) {
            true => Ok(()),
            false => self.expected(&format!("`{keyword}`")),
        }
    }

    /// Whether the next word is `name` and a `(` follows it: a function,
    /// where a field of that name would be compared.
    fn at_function(&mut self, name: &str) -> bool {
        self.skip_space();
        let word = self.peek_word();
        word.eq_ignore_ascii_case(name) && self.rest()[word.len()..].trim_start().starts_with('(')
    }

    fn end(&mut self) -> Parsed<()> {
        self.skip_space();
        match self.rest().is_empty() {
            true => Ok(()),
            false => self.expected("the end"),
        }
    }

    /// `[select paths] [where] [condition] [order by keys] [limit n]
    /// [offset n]` (SPEC §111.1).
    fn filter(&mut self) -> Parsed<Filter> {
        let mut filter = Filter::new();
        if self.keyword("select") {
            let mut paths = vec![self.path()?];
            while self.eat(",") {
                paths.push(self.path()?);
            }
            filter.select = Some(paths);
        }
        let condition = match self.keyword("where") {
            true => Some(self.condition()?),
            false if self.at_clause_end() => None,
            false if filter.select.is_some() => {
                return self.expected("`where` before the condition");
            }
            false => Some(self.condition()?),
        };
        filter.conditions = match condition {
            // `a and b`: each its own, as `Filter::and` adds them. A
            // lone `true` stays, so that it prints as it was written.
            Some(Condition::All(conditions)) if conditions.len() >= 2 => conditions,
            Some(condition) => vec![condition],
            None => Vec::new(),
        };
        if self.keyword("order") {
            self.expect_keyword("by")?;
            loop {
                let field = self.path()?;
                let order = match self.keyword("desc") {
                    true => SortOrder::Desc,
                    false => {
                        self.keyword("asc");
                        SortOrder::Asc
                    }
                };
                filter.sort.push(Sort { field, order });
                if !self.eat(",") {
                    break;
                }
            }
        }
        if self.keyword("limit") {
            filter.limit = Some(self.count()?);
        }
        if self.keyword("offset") {
            filter.skip = self.count()?;
        }
        Ok(filter)
    }

    /// Nothing but the clauses after a condition left: no condition.
    fn at_clause_end(&mut self) -> bool {
        self.skip_space();
        let word = self.peek_word();
        self.rest().is_empty()
            || ["order", "limit", "offset"]
                .iter()
                .any(|k| word.eq_ignore_ascii_case(k))
    }

    /// `or` binds least, then `and`, then `not` (SPEC §111.2).
    fn condition(&mut self) -> Parsed<Condition> {
        let mut any = vec![self.and()?];
        while self.keyword("or") {
            any.push(self.and()?);
        }
        Ok(one_or(any, Condition::Any))
    }

    fn and(&mut self) -> Parsed<Condition> {
        let mut all = vec![self.not()?];
        while self.keyword("and") {
            all.push(self.not()?);
        }
        Ok(one_or(all, Condition::All))
    }

    fn not(&mut self) -> Parsed<Condition> {
        match self.keyword("not") {
            true => Ok(Condition::Not(Box::new(self.not()?))),
            false => self.atom(),
        }
    }

    fn atom(&mut self) -> Parsed<Condition> {
        if self.eat("(") {
            let condition = self.condition()?;
            self.expect(")")?;
            return Ok(condition);
        }
        if self.keyword("true") {
            return Ok(Condition::All(Vec::new()));
        }
        if self.keyword("false") {
            return Ok(Condition::Any(Vec::new()));
        }
        if self.at_function("exists") {
            self.keyword("exists");
            self.expect("(")?;
            let field = self.path()?;
            self.expect(")")?;
            return Ok(Condition::Exists { field });
        }
        if self.at_function("size") {
            self.keyword("size");
            self.expect("(")?;
            let field = self.path()?;
            self.expect(")")?;
            let Some(op) = self.op() else {
                return self.expected("a comparison after `size(…)`");
            };
            let size = self.count()?;
            return Ok(Condition::Size { field, op, size });
        }
        let field = self.path()?;
        if self.keyword("is") {
            let op = match self.keyword("not") {
                true => Op::Ne,
                false => Op::Eq,
            };
            self.expect_keyword("null")?;
            let value = Document::Null;
            return Ok(Condition::Compare { field, op, value });
        }
        if self.keyword("has") {
            self.expect("(")?;
            let condition = Box::new(self.condition()?);
            self.expect(")")?;
            return Ok(Condition::ElemMatch { field, condition });
        }
        let Some(op) = self.op() else {
            return self.expected("a comparison (=, !=, <, <=, >, >=, contains, is null, has)");
        };
        let value = self.value()?;
        Ok(Condition::Compare { field, op, value })
    }

    fn op(&mut self) -> Option<Op> {
        self.skip_space();
        let symbols = [
            ("<=", Op::Lte),
            (">=", Op::Gte),
            ("!=", Op::Ne),
            ("<>", Op::Ne),
            ("=", Op::Eq),
            ("<", Op::Lt),
            (">", Op::Gt),
        ];
        for (symbol, op) in symbols {
            if self.rest().starts_with(symbol) {
                self.at += symbol.len();
                return Some(op);
            }
        }
        self.keyword("contains").then_some(Op::Contains)
    }

    /// A field path (SPEC §111.3): names, bare or in backticks, joined by
    /// `.`, each followed by any number of `[*]`; or `@`, the empty
    /// path, which inside `has` is the element itself.
    fn path(&mut self) -> Parsed<String> {
        self.skip_space();
        if self.rest().starts_with('@') {
            self.at += 1;
            if self.rest().starts_with(['.', '[']) {
                return self
                    .error("`@` is the element itself: a field of it is named without `@.`");
            }
            return Ok(String::new());
        }
        let mut path = String::new();
        loop {
            self.name(&mut path)?;
            while self.rest().starts_with("[*]") {
                path.push_str("[*]");
                self.at += 3;
            }
            if !self.rest().starts_with('.') {
                return Ok(path);
            }
            path.push('.');
            self.at += 1;
        }
    }

    /// One name of a path, onto `path`.
    fn name(&mut self, path: &mut String) -> Parsed<()> {
        if self.rest().starts_with('`') {
            let start = self.at;
            self.at += 1;
            loop {
                let rest = self.rest();
                let Some(tick) = rest.find('`') else {
                    return self.error_at(start, "a name in backticks isn't closed");
                };
                path.push_str(&rest[..tick]);
                self.at += tick + 1;
                // A doubled backtick is one in the name.
                match self.rest().starts_with('`') {
                    true => {
                        path.push('`');
                        self.at += 1;
                    }
                    false => return Ok(()),
                }
            }
        }
        let word = self.peek_word();
        if word.is_empty() {
            return self.expected("a field");
        }
        if is_reserved(word) {
            return self.error(format!(
                "expected a field, found the keyword `{word}`; a field of that name is written `` `{word}` ``"
            ));
        }
        path.push_str(word);
        self.at += word.len();
        Ok(())
    }

    /// A value (SPEC §111.4): JSON, tags and all, as `Document` reads it
    /// (§71), or `id("…")` or `date("…")`.
    fn value(&mut self) -> Parsed<Document> {
        self.skip_space();
        let start = self.at;
        let rest = self.rest();
        let len = match rest.chars().next() {
            Some('"') => string_len(rest),
            Some('[' | '{') => nested_len(rest),
            Some(c) if c == '-' || c.is_ascii_digit() => Some(number_len(rest)),
            _ => {
                return self.word_value();
            }
        };
        let Some(len) = len else {
            return self.error("a value isn't closed");
        };
        self.at += len;
        rest[..len]
            .parse()
            .or_else(|e| self.error_at(start, format!("not a value: {e}")))
    }

    fn word_value(&mut self) -> Parsed<Document> {
        if self.keyword("true") {
            return Ok(Document::Bool(true));
        }
        if self.keyword("false") {
            return Ok(Document::Bool(false));
        }
        if self.keyword("null") {
            return Ok(Document::Null);
        }
        if self.at_function("id") {
            self.keyword("id");
            let (start, text) = self.argument()?;
            return match text.parse::<DocId>() {
                Ok(id) => Ok(Document::Id(id)),
                Err(e) => self.error_at(start, format!("not an id: {e}")),
            };
        }
        if self.at_function("date") {
            self.keyword("date");
            let (start, text) = self.argument()?;
            return match text.parse::<DateTime>() {
                Ok(time) => Ok(Document::DateTime(time)),
                Err(e) => self.error_at(start, format!("not a date-time: {e}")),
            };
        }
        self.expected("a value")
    }

    /// `("…")`, the string inside, and where it starts.
    fn argument(&mut self) -> Parsed<(usize, String)> {
        self.expect("(")?;
        self.skip_space();
        let start = self.at;
        let Some(len) = self
            .rest()
            .starts_with('"')
            .then(|| string_len(self.rest()))
            .flatten()
        else {
            return self.expected("a string");
        };
        let text = serde_json::from_str(&self.rest()[..len])
            .or_else(|e| self.error_at(start, format!("not a string: {e}")))?;
        self.at += len;
        self.expect(")")?;
        Ok((start, text))
    }

    /// A count: `limit`, `offset`, `size`.
    fn count(&mut self) -> Parsed<usize> {
        self.skip_space();
        let rest = self.rest();
        let len = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if len == 0 {
            return self.expected("a number");
        }
        let n = rest[..len]
            .parse()
            .or_else(|_| self.error(format!("`{}` is too large", &rest[..len])))?;
        self.at += len;
        Ok(n)
    }
}

fn one_or(mut conditions: Vec<Condition>, many: fn(Vec<Condition>) -> Condition) -> Condition {
    match conditions.len() {
        1 => conditions.pop().expect("one"),
        _ => many(conditions),
    }
}

fn starts_word(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn in_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| word.eq_ignore_ascii_case(r))
}

/// The length of the JSON string `text` starts with, quotes included;
/// `None` if it isn't closed.
fn string_len(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, b) in text.bytes().enumerate().skip(1) {
        match b {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The length of the JSON array or object `text` starts with, as far as
/// its brackets go; whether it's JSON is for the parse to say.
fn nested_len(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = 0;
    while at < text.len() {
        match text.as_bytes()[at] {
            b'"' => at += string_len(&text[at..])? - 1,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The length of the number `text` starts with, as far as its characters
/// go; whether it's a number is for the parse to say.
fn number_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(text.len())
}

/// How tightly each form binds, to know where parentheses are needed.
const OR: u8 = 1;
const AND: u8 = 2;
const NOT: u8 = 3;
const ATOM: u8 = 4;

/// A filter as text (SPEC §111.6), which reads back as the same query.
impl fmt::Display for Filter {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut gap = "";
        if let Some(paths) = &self.select {
            f.write_str("select ")?;
            match paths.is_empty() {
                // Only `_id`, which `select` hands out anyway (§106).
                true => f.write_str("_id")?,
                false => write_list(f, paths, |f, path| write_path(f, path))?,
            }
            gap = " ";
        }
        if !self.conditions.is_empty() {
            f.write_str(gap)?;
            if self.select.is_some() {
                f.write_str("where ")?;
            }
            match self.conditions.as_slice() {
                [one] => write_condition(f, one, OR)?,
                all => {
                    for (i, condition) in all.iter().enumerate() {
                        if i > 0 {
                            f.write_str(" and ")?;
                        }
                        write_condition(f, condition, NOT)?;
                    }
                }
            }
            gap = " ";
        }
        if !self.sort.is_empty() {
            write!(f, "{gap}order by ")?;
            write_list(f, &self.sort, |f, sort| {
                write_path(f, &sort.field)?;
                match sort.order {
                    SortOrder::Asc => Ok(()),
                    SortOrder::Desc => f.write_str(" desc"),
                }
            })?;
            gap = " ";
        }
        if let Some(limit) = self.limit {
            write!(f, "{gap}limit {limit}")?;
            gap = " ";
        }
        if self.skip > 0 {
            write!(f, "{gap}offset {}", self.skip)?;
        }
        Ok(())
    }
}

/// A condition as text (SPEC §111.6), which reads back as the same one.
impl fmt::Display for Condition {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write_condition(f, self, OR)
    }
}

fn write_list<T>(
    f: &mut Formatter<'_>,
    items: &[T],
    mut write: impl FnMut(&mut Formatter<'_>, &T) -> fmt::Result,
) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write(f, item)?;
    }
    Ok(())
}

/// `condition`, in parentheses if it binds less tightly than `least`.
fn write_condition(f: &mut Formatter<'_>, condition: &Condition, least: u8) -> fmt::Result {
    let binds = match condition {
        Condition::Any(any) if any.len() >= 2 => OR,
        Condition::All(all) if all.len() >= 2 => AND,
        Condition::Any(one) | Condition::All(one) if one.len() == 1 => {
            return write_condition(f, &one[0], least);
        }
        Condition::Not(_) => NOT,
        _ => ATOM,
    };
    if binds < least {
        f.write_char('(')?;
    }
    match condition {
        Condition::Compare { field, op, value } => {
            write_path(f, field)?;
            match (op, value) {
                (Op::Eq, Document::Null) => f.write_str(" is null")?,
                (Op::Ne, Document::Null) => f.write_str(" is not null")?,
                _ => {
                    write!(f, " {} ", op_text(op))?;
                    write_value(f, value)?;
                }
            }
        }
        Condition::Exists { field } => {
            f.write_str("exists(")?;
            write_path(f, field)?;
            f.write_char(')')?;
        }
        Condition::Size { field, op, size } => {
            f.write_str("size(")?;
            write_path(f, field)?;
            write!(f, ") {} {size}", op_text(op))?;
        }
        Condition::ElemMatch { field, condition } => {
            write_path(f, field)?;
            f.write_str(" has (")?;
            write_condition(f, condition, OR)?;
            f.write_char(')')?;
        }
        Condition::All(all) if all.is_empty() => f.write_str("true")?,
        Condition::Any(any) if any.is_empty() => f.write_str("false")?,
        Condition::All(all) => write_joined(f, all, " and ", NOT)?,
        Condition::Any(any) => write_joined(f, any, " or ", AND)?,
        Condition::Not(inner) => {
            f.write_str("not ")?;
            write_condition(f, inner, NOT)?;
        }
    }
    if binds < least {
        f.write_char(')')?;
    }
    Ok(())
}

fn write_joined(
    f: &mut Formatter<'_>,
    conditions: &[Condition],
    with: &str,
    least: u8,
) -> fmt::Result {
    for (i, condition) in conditions.iter().enumerate() {
        if i > 0 {
            f.write_str(with)?;
        }
        write_condition(f, condition, least)?;
    }
    Ok(())
}

fn op_text(op: &Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "!=",
        Op::Lt => "<",
        Op::Lte => "<=",
        Op::Gt => ">",
        Op::Gte => ">=",
        Op::Contains => "contains",
    }
}

/// `path` as `path` reads it: names that aren't bare words, or are
/// reserved, in backticks; the empty path as `@`.
fn write_path(f: &mut Formatter<'_>, path: &str) -> fmt::Result {
    if path.is_empty() {
        return f.write_char('@');
    }
    for (i, step) in path.split('.').enumerate() {
        if i > 0 {
            f.write_char('.')?;
        }
        let name = step.trim_end_matches("[*]");
        let bare = name.starts_with(starts_word) && name.chars().all(in_word) && !is_reserved(name);
        match bare {
            true => f.write_str(name)?,
            false => write!(f, "`{}`", name.replace('`', "``"))?,
        }
        f.write_str(&step[name.len()..])?;
    }
    Ok(())
}

/// `value` as `value` reads it: tagged JSON (§71), but ids and
/// date-times by name.
fn write_value(f: &mut Formatter<'_>, value: &Document) -> fmt::Result {
    match value {
        Document::Id(id) => write!(f, "id(\"{id}\")"),
        Document::DateTime(time) => write!(f, "date(\"{time}\")"),
        other => write!(f, "{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::XorShift;

    fn parse(text: &str) -> Filter {
        text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn error(text: &str) -> String {
        match text.parse::<Filter>() {
            Ok(filter) => panic!("{text} parsed as {filter:?}"),
            Err(e) => e.to_string(),
        }
    }

    fn same(text: &str, filter: Filter) {
        assert_eq!(
            format!("{:?}", parse(text)),
            format!("{filter:?}"),
            "{text}"
        );
    }

    #[test]
    fn the_examples_read_as_their_builders() {
        same(
            r#"status = "Complete" and seen >= 10 order by started_at desc limit 1"#,
            Filter::new()
                .eq("status", "Complete")
                .gte("seen", 10)
                .sort_desc("started_at")
                .limit(1),
        );
        same(
            "select name, address.city where tags has (@ = \"urgent\") or not exists(done)",
            Filter::new()
                .and(
                    Condition::elem_match("tags", Condition::eq("", "urgent"))
                        | Condition::missing("done"),
                )
                .select(["name", "address.city"]),
        );
        same(
            "order by name, born desc limit 20 offset 40",
            Filter::new()
                .sort_asc("name")
                .then_desc("born")
                .skip(40)
                .limit(20),
        );
        same(
            r#"items has (sku = "A1" and qty > 2) and size(items) <= 3 and x is null and y is not null"#,
            Filter::new()
                .elem_match(
                    "items",
                    Condition::eq("sku", "A1") & Condition::gt("qty", 2),
                )
                .size("items", Op::Lte, 3)
                .is_null("x")
                .is_not_null("y"),
        );
        same(
            r#"a != 1 and b <> 2 and c < 3 and d contains "x" and e = [1, 2.0] and f = {"g": null} and h = ["]", "\"}"]"#,
            Filter::new()
                .ne("a", 1)
                .ne("b", 2)
                .lt("c", 3)
                .contains("d", "x")
                .eq("e", Document::Array(vec![1.into(), 2.0.into()]))
                .eq("f", r#"{"g": null}"#.parse::<Document>().unwrap())
                .eq("h", Document::Array(vec!["]".into(), "\"}".into()])),
        );
        same("", Filter::new());
        same("  where a = 1  ", Filter::new().eq("a", 1));
    }

    #[test]
    fn ids_and_date_times_have_their_own_literals() {
        let id = DocId::NIL;
        let time = DateTime::from_unix(1_714_564_800, 5).unwrap();
        let text = format!(r#"_id = id("{id}") and at < date("{time}")"#);
        same(&text, Filter::new().id(id).lt("at", time));
        // The tags of §71 are values too.
        let tagged = format!(r#"_id = {{"$id": "{id}"}} and at < {{"$date": "{time}"}}"#);
        same(&tagged, Filter::new().id(id).lt("at", time));
        assert_eq!(parse(&tagged).to_string(), text);
    }

    #[test]
    fn not_binds_tighter_than_and_than_or() {
        let (a, b, c) = (
            Condition::eq("a", 1),
            Condition::eq("b", 2),
            Condition::eq("c", 3),
        );
        let read = |text: &str| text.parse::<Condition>().unwrap();
        assert_eq!(
            read("a = 1 or b = 2 and c = 3"),
            a.clone() | (b.clone() & c.clone())
        );
        assert_eq!(
            read("(a = 1 or b = 2) and c = 3"),
            (a.clone() | b.clone()) & c.clone()
        );
        assert_eq!(read("not a = 1 and b = 2"), !a.clone() & b.clone());
        assert_eq!(read("not (a = 1 and b = 2)"), !(a.clone() & b.clone()));
        assert_eq!(read("not not a = 1"), !!a.clone());
        assert_eq!(
            read("a = 1 or b = 2 or c = 3").to_string(),
            "a = 1 or b = 2 or c = 3"
        );
        assert_eq!(
            (a.clone() & (b.clone() | c.clone())).to_string(),
            "a = 1 and (b = 2 or c = 3)"
        );
        assert_eq!(
            (!(a.clone() | b.clone())).to_string(),
            "not (a = 1 or b = 2)"
        );
        assert_eq!(read("true"), Condition::all([]));
        assert_eq!(
            read("false or a = 1"),
            Condition::Any(vec![Condition::any([]), a])
        );
    }

    #[test]
    fn keywords_are_any_case_and_names_in_backticks_any_name() {
        same(
            "SELECT a WHERE b IS NOT NULL AND c CONTAINS \"x\" ORDER BY d DESC LIMIT 1 OFFSET 2",
            Filter::new()
                .is_not_null("b")
                .contains("c", "x")
                .sort_desc("d")
                .limit(1)
                .skip(2)
                .select(["a"]),
        );
        // Words that mean something only where a field can't be are fields.
        same(
            "size > 3 and exists = 1 and is = 2 and date = date(\"2024-01-01T00:00:00Z\") order by asc desc",
            Filter::new()
                .gt("size", 3)
                .eq("exists", 1)
                .eq("is", 2)
                .eq("date", "2024-01-01T00:00:00Z".parse::<DateTime>().unwrap())
                .sort_desc("asc"),
        );
        same(
            "`order` = 1 and `first name`.`a.b` = 2 and `tick``s`[*] = 3 and ``[*].x = 4 and `@` = 5",
            Filter::new()
                .eq("order", 1)
                .eq("first name.a.b", 2)
                .eq("tick`s[*]", 3)
                .eq("[*].x", 4)
                .eq("@", 5),
        );
        let filter = Filter::new()
            .eq("order", 1)
            .eq("first name", 2)
            .eq("tick`s[*]", 3)
            .eq("Ünïcode", 4);
        assert_eq!(
            filter.to_string(),
            "`order` = 1 and `first name` = 2 and `tick``s`[*] = 3 and Ünïcode = 4"
        );
    }

    #[test]
    fn mistakes_are_named_where_they_are() {
        assert_eq!(
            error("a = "),
            "expected a value, found the end at line 1 column 5"
        );
        assert_eq!(
            error("a = 1 b = 2"),
            "expected the end, found `b` at line 1 column 7"
        );
        assert_eq!(
            error("a == 1"),
            "expected a value, found `=` at line 1 column 4"
        );
        assert_eq!(
            error("a = 1\n  and order = 1"),
            "expected a field, found the keyword `order`; a field of that name is written `` `order` `` at line 2 column 7"
        );
        assert_eq!(
            error("a"),
            "expected a comparison (=, !=, <, <=, >, >=, contains, is null, has), found the end at line 1 column 2"
        );
        assert_eq!(
            error("select a b = 1"),
            "expected `where` before the condition, found `b` at line 1 column 10"
        );
        assert_eq!(
            error("(a = 1"),
            "expected `)`, found the end at line 1 column 7"
        );
        assert_eq!(
            error("order name"),
            "expected `by`, found `name` at line 1 column 7"
        );
        assert_eq!(
            error("limit -1"),
            "expected a number, found `-` at line 1 column 7"
        );
        assert_eq!(
            error("limit 99999999999999999999999"),
            "`99999999999999999999999` is too large at line 1 column 7"
        );
        assert_eq!(
            error("a = 'x'"),
            "expected a value, found `'` at line 1 column 5"
        );
        assert_eq!(error("a = \"x"), "a value isn't closed at line 1 column 5");
        assert_eq!(
            error("`a = 1"),
            "a name in backticks isn't closed at line 1 column 1"
        );
        assert_eq!(
            error("@.a = 1"),
            "`@` is the element itself: a field of it is named without `@.` at line 1 column 2"
        );
        assert!(error("a = 18446744073709551615").starts_with("not a value: "));
        assert!(error("a = [1,]").starts_with("not a value: "));
        assert!(error("a = id(\"nope\")").starts_with("not an id: "));
        assert!(error("a = date(\"nope\")").ends_with("at line 1 column 10"));
        assert_eq!(
            error("where"),
            "expected a field, found the end at line 1 column 6"
        );
        let e = "a = 1 and ü ?".parse::<Filter>().unwrap_err();
        assert_eq!(
            (e.offset(), e.to_string().ends_with("column 13")),
            (13, true)
        );
    }

    #[test]
    fn a_parsed_filter_reads_what_its_builder_reads() {
        let docs: Vec<Document> = (0..30)
            .map(|i| {
                format!(
                    r#"{{"n": {i}, "s": "v{}", "tags": ["t{}", "u"], "o": {{"k": {}}}}}"#,
                    i % 4,
                    i % 3,
                    i % 5
                )
                .parse()
                .unwrap()
            })
            .collect();
        let text = r#"select n, o.k where (s = "v1" or tags has (@ = "t2")) and not o.k > 3 order by o.k desc, n limit 5 offset 1"#;
        let built = Filter::new()
            .any_of([
                Condition::eq("s", "v1"),
                Condition::elem_match("tags", Condition::eq("", "t2")),
            ])
            .and(!Condition::gt("o.k", 3))
            .sort_desc("o.k")
            .then_asc("n")
            .skip(1)
            .limit(5)
            .select(["n", "o.k"]);
        let read = parse(text).apply(docs.clone());
        assert_eq!(read, built.apply(docs));
        assert_eq!(read.len(), 5);
        assert_eq!(built.to_string(), text);
    }

    /// Random filters of every shape, values and names of every kind:
    /// each one's text reads back as a filter with the same text, which
    /// picks the same documents in the same order.
    #[test]
    fn every_filter_reads_back_from_its_text() {
        let mut rng = XorShift(0x5eed_1111);
        let docs: Vec<Document> = (0..40).map(|_| random_doc(&mut rng, 2)).collect();
        for round in 0..3000 {
            let filter = random_filter(&mut rng);
            let text = filter.to_string();
            let read: Filter = text
                .parse()
                .unwrap_or_else(|e| panic!("round {round}: {text}: {e}"));
            assert_eq!(read.to_string(), text, "round {round}");
            let again: Filter = read.to_string().parse().unwrap();
            assert_eq!(
                format!("{again:?}"),
                format!("{read:?}"),
                "round {round}: {text}"
            );
            let (want, got) = (filter.apply(docs.clone()), read.apply(docs.clone()));
            assert_eq!(
                format!("{got:?}"),
                format!("{want:?}"),
                "round {round}: {text}"
            );
        }
    }

    const NAMES: [&str; 9] = [
        "a",
        "b",
        "tags",
        "o",
        "order",
        "first name",
        "",
        "tick`",
        "Größe",
    ];

    fn random_path(rng: &mut XorShift) -> String {
        let steps = 1 + rng.below(3);
        let mut path = String::new();
        for i in 0..steps {
            if i > 0 {
                path.push('.');
            }
            path.push_str(NAMES[rng.below(NAMES.len())]);
            if rng.below(4) == 0 {
                path.push_str("[*]");
            }
        }
        path
    }

    fn random_value(rng: &mut XorShift, depth: usize) -> Document {
        match rng.below(if depth == 0 { 9 } else { 11 }) {
            0 => Document::Null,
            1 => Document::Bool(rng.below(2) == 0),
            2 => Document::Int(rng.below(7) as i64 - 3),
            3 => Document::Float([0.5, -2.0, 1e300, f64::NAN, f64::INFINITY, 3.0][rng.below(6)]),
            4 => Document::String(
                ["", "u", "t1", "v2", "q\"uo\\te", "Ünï\n", "]}\"[{"][rng.below(7)].into(),
            ),
            5 => Document::Id(DocId([rng.below(3) as u8; 16])),
            6 => Document::DateTime(
                DateTime::from_unix(rng.below(3) as i64, rng.below(2) as u32).unwrap(),
            ),
            7 => Document::Binary(vec![rng.below(256) as u8; rng.below(3)]),
            8 => Document::Int(i64::MIN),
            9 => Document::Array(
                (0..rng.below(3))
                    .map(|_| random_value(rng, depth - 1))
                    .collect(),
            ),
            _ => Document::Object(
                (0..rng.below(3))
                    .map(|_| {
                        (
                            NAMES[rng.below(NAMES.len())].to_string(),
                            random_value(rng, depth - 1),
                        )
                    })
                    .collect(),
            ),
        }
    }

    fn random_doc(rng: &mut XorShift, depth: usize) -> Document {
        Document::Object(
            (0..1 + rng.below(4))
                .map(|_| {
                    (
                        NAMES[rng.below(NAMES.len())].to_string(),
                        random_value(rng, depth),
                    )
                })
                .collect(),
        )
    }

    fn random_op(rng: &mut XorShift) -> Op {
        [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::Contains,
        ][rng.below(7)]
        .clone()
    }

    fn random_condition(rng: &mut XorShift, depth: usize) -> Condition {
        let kinds = if depth == 0 { 4 } else { 8 };
        let few = |rng: &mut XorShift| {
            (0..rng.below(4))
                .map(|_| random_condition(rng, depth - 1))
                .collect()
        };
        match rng.below(kinds) {
            0 | 1 => Condition::compare(random_path(rng), random_op(rng), random_value(rng, 1)),
            2 => Condition::exists(random_path(rng)),
            3 => Condition::size(random_path(rng), random_op(rng), rng.below(3)),
            4 => Condition::elem_match(random_path(rng), random_condition(rng, depth - 1)),
            5 => Condition::All(few(rng)),
            6 => Condition::Any(few(rng)),
            _ => !random_condition(rng, depth - 1),
        }
    }

    fn random_filter(rng: &mut XorShift) -> Filter {
        let mut filter = Filter::new();
        for _ in 0..rng.below(3) {
            filter = filter.and(random_condition(rng, 3));
        }
        for _ in 0..rng.below(3) {
            let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
            filter = filter.then_by(random_path(rng), order);
        }
        if rng.below(3) == 0 {
            filter = filter.limit(rng.below(5));
        }
        if rng.below(3) == 0 {
            filter = filter.skip(rng.below(5));
        }
        if rng.below(3) == 0 {
            filter = filter.select(
                (0..rng.below(3))
                    .map(|_| random_path(rng))
                    .collect::<Vec<_>>(),
            );
        }
        filter
    }
}
