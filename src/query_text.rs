//! Filters and conditions as text (SPEC §111), and aggregates (§157):
//! `FromStr` reads the SQL-like language into a `Filter`, `Condition` or
//! `Aggregate`, `Display` writes one back. The text says what the
//! builders say, no more: it compiles to the same filter, which the
//! planner sees as it sees any other.

use crate::aggregate::{Column, Kind, Window};
use crate::datetime::DateTime;
use crate::document::{DocId, Document};
use crate::query::{Aggregate, Condition, Filter, Hint, Op, Sort, SortOrder};
use std::fmt::{self, Formatter, Write};
use std::str::FromStr;
use std::time::Duration;

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

/// An aggregate from its text (SPEC §157), SQL's select list, `GROUP BY`
/// and what follows it, for `Collection::aggregate` beside a filter:
///
/// ```
/// use trunkdb::query::Aggregate;
///
/// let per_team: Aggregate = r#"
///     count() as n, avg(hours) as mean,
///     count() filter (where status = "open") as open,
///     last(status order by changed) as latest
///     group by team
///     having n > 10
///     order by n desc
///     limit 5
/// "#.parse()?;
/// # Ok::<(), trunkdb::query::ParseFilterError>(())
/// ```
///
/// `[window <path> <duration>] [unwind <paths>] [<column>, …] [group by
/// <paths>] [having <condition>] [order by <columns>] [limit <n>]
/// [offset <n>]`, in that order. A column is `count()`, `sum(p)`,
/// `min(p)`, `max(p)`, `avg(p)`, `count(distinct p)`, `first(p)` or
/// `last(p)`, `first` and `last` with `order by <path>` inside, then
/// `filter (where <condition>)` if it takes in only some documents, then
/// `as <name>`. A duration is a whole number and a unit: `ns`, `us`,
/// `ms`, `s`, `m`, `h` or `d`, `5m`. These words mean something only
/// here, and only where a path can't be, so filters keep theirs.
impl FromStr for Aggregate {
    type Err = ParseFilterError;

    fn from_str(text: &str) -> Result<Aggregate, ParseFilterError> {
        let mut parser = Parser { text, at: 0 };
        let aggregate = parser.aggregate()?;
        parser.end()?;
        Ok(aggregate)
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

    /// `[select (paths | *) [except paths]] [where] [condition] [order by
    /// keys] [limit n] [offset n]` (SPEC §111.1, §143). `except` means
    /// something only after `select`'s paths or `*`, where a field can't
    /// be, so it isn't reserved.
    fn filter(&mut self) -> Parsed<Filter> {
        let mut filter = Filter::new();
        let selects = self.keyword("select");
        if selects {
            if !self.eat("*") {
                filter.select = Some(self.paths()?);
            }
            if self.keyword("except") {
                filter.except = self.paths()?;
            }
        }
        let condition = match self.keyword("where") {
            true => Some(self.condition()?),
            false if self.at_clause_end() => None,
            false if selects => {
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
            filter.sort = self.sort_keys()?;
        }
        if self.keyword("limit") {
            filter.limit = Some(self.count()?);
        }
        if self.keyword("offset") {
            filter.skip = self.count()?;
        }
        filter.hint = self.hint()?;
        Ok(filter)
    }

    /// The keys after `order by`, each `asc` or `desc`, between commas.
    fn sort_keys(&mut self) -> Parsed<Vec<Sort>> {
        let mut keys = Vec::new();
        loop {
            // What `Display` writes of a `Filter::sort_by_key` (SPEC §154).
            if self.at_function("key") {
                return self.error(
                    "`key(…)` is a closure in code, which has no text: build the filter in code",
                );
            }
            let field = self.path()?;
            let order = match self.keyword("desc") {
                true => SortOrder::Desc,
                false => {
                    self.keyword("asc");
                    SortOrder::Asc
                }
            };
            keys.push(Sort { field, order });
            if !self.eat(",") {
                return Ok(keys);
            }
        }
    }

    /// An aggregate's text (SPEC §157), as `FromStr for Aggregate` says.
    fn aggregate(&mut self) -> Parsed<Aggregate> {
        let mut aggregate = Aggregate::new();
        if self.keyword("window") {
            let path = self.path()?;
            let duration = self.duration()?;
            aggregate.window = Some(Window { path, duration });
        }
        if self.keyword("unwind") {
            aggregate.unwind = self.paths()?;
        }
        if self.at_column() {
            loop {
                aggregate.columns.push(self.column()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        if self.keyword("group") {
            self.expect_keyword("by")?;
            aggregate.group_by = self.paths()?;
        }
        if self.keyword("having") {
            aggregate.rows.conditions = match self.condition()? {
                Condition::All(conditions) if conditions.len() >= 2 => conditions,
                condition => vec![condition],
            };
        }
        if self.keyword("order") {
            self.expect_keyword("by")?;
            aggregate.rows.sort = self.sort_keys()?;
        }
        if self.keyword("limit") {
            aggregate.rows.limit = Some(self.count()?);
        }
        if self.keyword("offset") {
            aggregate.rows.skip = self.count()?;
        }
        Ok(aggregate)
    }

    /// Whether a column starts here: a column's function and its `(`.
    fn at_column(&mut self) -> bool {
        COLUMNS.iter().any(|(name, _)| self.at_function(name))
    }

    /// `count()`, `sum(p)` and the rest, then `filter (where …)` if any,
    /// then `as <name>`.
    fn column(&mut self) -> Parsed<Column> {
        self.skip_space();
        let word = self.peek_word();
        let Some(&(_, kind)) = (COLUMNS.iter())
            .find(|(name, _)| word.eq_ignore_ascii_case(name) && self.at_function(name))
        else {
            return self.expected(
                "a column: count(), sum(…), min(…), max(…), avg(…), count(distinct …), first(…) or last(…)",
            );
        };
        self.at += word.len();
        self.expect("(")?;
        let (kind, path) = match kind {
            Kind::Count if self.keyword("distinct") => (Kind::CountDistinct, Some(self.path()?)),
            Kind::Count => {
                if !self.eat("*") && !self.rest().trim_start().starts_with(')') {
                    return self.error(
                        "count() takes no path: it counts the documents. Those with a value \
                         at a path: count() filter (where exists(p)); different values: \
                         count(distinct p)",
                    );
                }
                (Kind::Count, None)
            }
            kind => (kind, Some(self.path()?)),
        };
        let by = match kind {
            Kind::First | Kind::Last if self.keyword("order") => {
                self.expect_keyword("by")?;
                Some(self.path()?)
            }
            _ => None,
        };
        self.expect(")")?;
        let condition = match self.keyword("filter") {
            true => {
                self.expect("(")?;
                self.expect_keyword("where")?;
                let condition = self.condition()?;
                self.expect(")")?;
                Some(condition)
            }
            false => None,
        };
        if !self.keyword("as") {
            return self.expected("`as` and the column's name");
        }
        let name = self.path()?;
        Ok(Column {
            name,
            kind,
            path,
            condition,
            by,
        })
    }

    /// A duration: a whole number and its unit, `5m` (SPEC §157).
    fn duration(&mut self) -> Parsed<Duration> {
        self.skip_space();
        let start = self.at;
        let n = self.count()?;
        let rest = self.rest();
        let Some(&(unit, nanos)) = (UNITS.iter())
            .filter(|(unit, _)| rest.starts_with(unit))
            .max_by_key(|(unit, _)| unit.len())
            .filter(|(unit, _)| !rest[unit.len()..].starts_with(in_word))
        else {
            return self.expected("a unit after the number: ns, us, ms, s, m, h or d");
        };
        self.at += unit.len();
        match (n as u128).checked_mul(nanos) {
            Some(total) if total <= u64::MAX as u128 * 1_000_000_000 => {
                let secs = (total / 1_000_000_000) as u64;
                Ok(Duration::new(secs, (total % 1_000_000_000) as u32))
            }
            _ => self.error_at(start, "the duration is too long"),
        }
    }

    /// Whether `use index` or `use no index` is next (SPEC §132.4): two
    /// words, so that a field named `use` stays free.
    fn at_hint(&mut self) -> bool {
        let start = self.at;
        let hint = self.keyword("use") && (self.keyword("index") || self.keyword("no"));
        self.at = start;
        hint
    }

    /// `use index <fields>` or `use no index`, if next: `use_index` and
    /// `no_index` (SPEC §132.4). The fields as an index is named, one
    /// path or several in parentheses: `use index (status, created)`.
    fn hint(&mut self) -> Parsed<Option<Hint>> {
        if !self.at_hint() {
            return Ok(None);
        }
        self.keyword("use");
        if self.keyword("no") {
            self.expect_keyword("index")?;
            return Ok(Some(Hint::NoIndex));
        }
        self.keyword("index");
        if !self.eat("(") {
            return Ok(Some(Hint::Index(vec![self.path()?])));
        }
        let mut fields = vec![self.path()?];
        while self.eat(",") {
            fields.push(self.path()?);
        }
        self.expect(")")?;
        Ok(Some(Hint::Index(fields)))
    }

    /// Nothing but the clauses after a condition left: no condition.
    /// Paths, one or more, between commas.
    fn paths(&mut self) -> Parsed<Vec<String>> {
        let mut paths = vec![self.path()?];
        while self.eat(",") {
            paths.push(self.path()?);
        }
        Ok(paths)
    }

    fn at_clause_end(&mut self) -> bool {
        self.skip_space();
        let word = self.peek_word();
        self.rest().is_empty()
            || ["order", "limit", "offset"]
                .iter()
                .any(|k| word.eq_ignore_ascii_case(k))
            || self.at_hint()
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
        // What `Display` writes of a `Condition::custom` (SPEC §136.5).
        if self.at_function("custom") {
            return self.error(
                "`custom(…)` is a closure in code, which has no text: build the filter in code",
            );
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
        if self.keyword("in") {
            return self.list(field);
        }
        if self.keyword("not") {
            self.expect_keyword("in")?;
            return Ok(Condition::Not(Box::new(self.list(field)?)));
        }
        if self.keyword("has") {
            self.expect("(")?;
            let condition = Box::new(self.condition()?);
            self.expect(")")?;
            return Ok(Condition::ElemMatch { field, condition });
        }
        let Some(op) = self.op() else {
            return self.expected(
                "a comparison (=, !=, <, <=, >, >=, contains, starts with, is null, in, has)",
            );
        };
        let value = self.value()?;
        Ok(Condition::Compare { field, op, value })
    }

    /// `(value, …)` after `in` (SPEC §115): an `eq` of `field` per value,
    /// as `Condition::is_in` makes them.
    fn list(&mut self, field: String) -> Parsed<Condition> {
        self.expect("(")?;
        let mut values = Vec::new();
        if !self.eat(")") {
            loop {
                values.push(self.value()?);
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(")")?;
        }
        Ok(Condition::is_in(field, values))
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
        if self.keyword("contains") {
            return Some(Op::Contains);
        }
        // Two words (SPEC §158.3): `starts` alone is no operator.
        let before = self.at;
        if self.keyword("starts") {
            if self.keyword("with") {
                return Some(Op::StartsWith);
            }
            self.at = before;
        }
        None
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

/// An aggregate as text (SPEC §157), which reads back to the same one:
/// each clause in its order, a column's function and its name.
impl fmt::Display for Aggregate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut gap = "";
        if let Some(window) = &self.window {
            f.write_str("window ")?;
            write_path(f, &window.path)?;
            write!(f, " {}", DurationText(window.duration))?;
            gap = " ";
        }
        if !self.unwind.is_empty() {
            write!(f, "{gap}unwind ")?;
            write_list(f, &self.unwind, |f, path| write_path(f, path))?;
            gap = " ";
        }
        if !self.columns.is_empty() {
            f.write_str(gap)?;
            write_list(f, &self.columns, write_column)?;
            gap = " ";
        }
        if !self.group_by.is_empty() {
            write!(f, "{gap}group by ")?;
            write_list(f, &self.group_by, |f, path| write_path(f, path))?;
            gap = " ";
        }
        let rows = &self.rows;
        if !rows.conditions.is_empty() {
            write!(f, "{gap}having ")?;
            match rows.conditions.as_slice() {
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
        if !rows.sort.is_empty() {
            write!(f, "{gap}order by ")?;
            write_list(f, &rows.sort, |f, sort| {
                write_path(f, &sort.field)?;
                match sort.order {
                    SortOrder::Asc => Ok(()),
                    SortOrder::Desc => f.write_str(" desc"),
                }
            })?;
            gap = " ";
        }
        if let Some(limit) = rows.limit {
            write!(f, "{gap}limit {limit}")?;
            gap = " ";
        }
        if rows.skip > 0 {
            write!(f, "{gap}offset {}", rows.skip)?;
        }
        Ok(())
    }
}

/// One column: its function, `filter (where …)`, `as` its name.
fn write_column(f: &mut Formatter<'_>, column: &Column) -> fmt::Result {
    let function = match column.kind {
        Kind::Count | Kind::CountDistinct => "count",
        Kind::Sum => "sum",
        Kind::Min => "min",
        Kind::Max => "max",
        Kind::Avg => "avg",
        Kind::First => "first",
        Kind::Last => "last",
    };
    write!(f, "{function}(")?;
    if column.kind == Kind::CountDistinct {
        f.write_str("distinct ")?;
    }
    if let Some(path) = &column.path {
        write_path(f, path)?;
    }
    if let Some(by) = &column.by {
        f.write_str(" order by ")?;
        write_path(f, by)?;
    }
    f.write_char(')')?;
    if let Some(condition) = &column.condition {
        f.write_str(" filter (where ")?;
        write_condition(f, condition, OR)?;
        f.write_char(')')?;
    }
    f.write_str(" as ")?;
    write_path(f, &column.name)
}

/// A duration as the parser reads it: the longest unit it is a whole
/// number of, `300s` as `5m`.
struct DurationText(Duration);

impl fmt::Display for DurationText {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let nanos = self.0.as_nanos();
        let (unit, size) = (UNITS.iter())
            .find(|(_, size)| nanos.is_multiple_of(*size))
            .expect("every duration is whole nanoseconds");
        match nanos {
            0 => f.write_str("0s"),
            _ => write!(f, "{}{unit}", nanos / size),
        }
    }
}

/// The functions an aggregate's column is (SPEC §157); `count`'s
/// `distinct` makes it `count_distinct`.
const COLUMNS: [(&str, Kind); 7] = [
    ("count", Kind::Count),
    ("sum", Kind::Sum),
    ("min", Kind::Min),
    ("max", Kind::Max),
    ("avg", Kind::Avg),
    ("first", Kind::First),
    ("last", Kind::Last),
];

/// A duration's units and their length in nanoseconds, longest first:
/// `Display` writes the longest that is whole.
const UNITS: [(&str, u128); 7] = [
    ("d", 86_400_000_000_000),
    ("h", 3_600_000_000_000),
    ("m", 60_000_000_000),
    ("s", 1_000_000_000),
    ("ms", 1_000_000),
    ("us", 1_000),
    ("ns", 1),
];

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
        let selects = self.select.is_some() || !self.except.is_empty();
        if selects {
            f.write_str("select ")?;
            match self.select.as_deref() {
                None => f.write_str("*")?,
                // Only `_id`, which `select` hands out anyway (§106).
                Some([]) => f.write_str("_id")?,
                Some(paths) => write_list(f, paths, |f, path| write_path(f, path))?,
            }
            if !self.except.is_empty() {
                f.write_str(" except ")?;
                write_list(f, &self.except, |f, path| write_path(f, path))?;
            }
            gap = " ";
        }
        if !self.conditions.is_empty() {
            f.write_str(gap)?;
            if selects {
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
        if self.sorted() {
            write!(f, "{gap}order by ")?;
            // What the parser refuses (SPEC §154): a closure has no text.
            if let Some(by_key) = &self.by_key {
                f.write_str("key(")?;
                write_list(f, by_key.fields(), |f, path| write_path(f, path))?;
                f.write_char(')')?;
                if by_key.order() == SortOrder::Desc {
                    f.write_str(" desc")?;
                }
                if !self.sort.is_empty() {
                    f.write_str(", ")?;
                }
            }
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
            gap = " ";
        }
        match &self.hint {
            None => {}
            Some(Hint::NoIndex) => write!(f, "{gap}use no index")?,
            Some(Hint::Index(fields)) => {
                write!(f, "{gap}use index ")?;
                match &fields[..] {
                    [one] => write_path(f, one)?,
                    several => {
                        f.write_char('(')?;
                        write_list(f, several, |f, path| write_path(f, path))?;
                        f.write_char(')')?;
                    }
                }
            }
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
    // An OR of `eq`s of one field is `in`, and its NOT `not in` (SPEC
    // §115): one comparison each, needing no parentheses.
    if let Some((field, values)) = in_list(condition) {
        return write_in(f, field, " in (", &values);
    }
    if let Condition::Not(inner) = condition
        && let Some((field, values)) = in_list(only(inner))
    {
        return write_in(f, field, " not in (", &values);
    }
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
        // The one condition that doesn't read back (SPEC §136.5): the
        // parser refuses `custom(`, saying a closure has no text.
        Condition::Custom(custom) => {
            f.write_str("custom(")?;
            write_list(f, custom.fields(), |f, path| write_path(f, path))?;
            f.write_char(')')?;
        }
    }
    if binds < least {
        f.write_char(')')?;
    }
    Ok(())
}

/// `condition`, past any AND or OR of it alone, which print as it does.
fn only(mut condition: &Condition) -> &Condition {
    while let Condition::All(one) | Condition::Any(one) = condition
        && let [inner] = one.as_slice()
    {
        condition = inner;
    }
    condition
}

/// The field and values of an OR of two or more `eq`s of one field. A
/// branch is looked at past an AND or OR of it alone, as it prints: else
/// `Any([All([a = 1]), a = 2])` printed `a = 1 or a = 2`, which reads
/// back as `in` and prints so, and the text didn't read back the same.
fn in_list(condition: &Condition) -> Option<(&str, Vec<&Document>)> {
    let Condition::Any(any) = only(condition) else {
        return None;
    };
    let mut field = None;
    let mut values = Vec::new();
    for branch in any {
        let Condition::Compare {
            field: this,
            op: Op::Eq,
            value,
        } = only(branch)
        else {
            return None;
        };
        if field.is_some_and(|field| field != this) {
            return None;
        }
        field = Some(this.as_str());
        values.push(value);
    }
    (values.len() >= 2).then_some((field?, values))
}

fn write_in(
    f: &mut Formatter<'_>,
    field: &str,
    keyword: &str,
    values: &[&Document],
) -> fmt::Result {
    write_path(f, field)?;
    f.write_str(keyword)?;
    write_list(f, values, |f, value| write_value(f, value))?;
    f.write_char(')')
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
        Op::StartsWith => "starts with",
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

    /// SPEC §143: `select *` and `except` after it or after paths, a
    /// field named `except` where a field can be, printed back as it
    /// reads.
    #[test]
    fn except_reads_after_select() {
        same("select * except body", Filter::new().except(["body"]));
        same(
            "SELECT * EXCEPT body, meta.raw WHERE a = 1",
            Filter::new().except(["body", "meta.raw"]).eq("a", 1),
        );
        same(
            "select address except address.zip order by n",
            Filter::new()
                .select(["address"])
                .except(["address.zip"])
                .sort_asc("n"),
        );
        same("select * where a = 1", Filter::new().eq("a", 1));
        same("select except", Filter::new().select(["except"]));
        same("except = 1", Filter::new().eq("except", 1));
        same(
            "select * except `except`, @",
            Filter::new().except(["except", ""]),
        );
        for text in [
            "select * except body",
            "select * except body, meta.raw where a = 1",
            "select address except address.zip order by n",
            "select _id except a",
            "select * except @",
        ] {
            assert_eq!(parse(text).to_string(), text);
        }
        // Not reserved: a field named `except` needs no backticks.
        let named = Filter::new().except(["except"]);
        assert_eq!(named.to_string(), "select * except except");
        same("select * except except", named);
        for bad in [
            "select * except",
            "select * a = 1",
            "select * except a b = 1",
        ] {
            error(bad);
        }
    }

    /// A hint last (SPEC §132.4): `use index` with a path or several in
    /// parentheses, `use no index`; printed back as it reads.
    #[test]
    fn a_hint_reads_as_use_index_and_no_index() {
        same(
            r#"status = "done" and created > 5 use index created"#,
            Filter::new()
                .eq("status", "done")
                .gt("created", 5)
                .use_index("created"),
        );
        same(
            "a = 1 order by b limit 2 offset 3 USE INDEX (a, b.c)",
            Filter::new()
                .eq("a", 1)
                .sort_asc("b")
                .limit(2)
                .skip(3)
                .use_index(["a", "b.c"]),
        );
        same("use no index", Filter::new().no_index());
        same(
            "select a use index `first name`",
            Filter::new().select(["a"]).use_index("first name"),
        );
        for text in [
            r#"status = "done" use index created"#,
            "a = 1 order by b limit 2 offset 3 use index (a, b.c)",
            "use no index",
            "offset 3 use no index",
            "use index _id",
            "select a use index `first name`",
            "tags[*] = 1 use index tags[*]",
        ] {
            assert_eq!(parse(text).to_string(), text);
        }
        // `use` without `index` or `no` after it is a field.
        same("use = 1", Filter::new().eq("use", 1));
        same(
            "use = 1 use index use",
            Filter::new().eq("use", 1).use_index("use"),
        );
        same(
            "index = 1 or no = 2",
            "index = 1 or no = 2".parse().unwrap(),
        );
        assert!(error("a = 1 use index").contains("expected"));
        assert!(error("a = 1 use no").contains("expected `index`"));
        assert!(error("a = 1 use index (a, b").contains("expected `)`"));
        // Last, after `offset`: anywhere else it's not the end.
        assert!(error("a = 1 use no index limit 2").contains("expected the end"));
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

    /// `in` and `not in` (SPEC §115): an OR of `eq`s of one field, read
    /// and printed as a list.
    #[test]
    fn a_list_of_values_reads_as_an_or_of_eqs_and_prints_as_in() {
        same(
            r#"status in ("open", "blocked") and n not in (1, 2.0, null)"#,
            Filter::new()
                .is_in("status", ["open", "blocked"])
                .and(!Condition::is_in(
                    "n",
                    [Document::Int(1), Document::Float(2.0), Document::Null],
                )),
        );
        let read = |text: &str| text.parse::<Condition>().unwrap();
        assert_eq!(
            read(r#"status in ("open", "blocked")"#),
            Condition::eq("status", "open") | Condition::eq("status", "blocked")
        );
        assert_eq!(read("a IN ()"), Condition::any([]));
        assert_eq!(
            read("`in` in (1)"),
            Condition::Any(vec![Condition::eq("in", 1)])
        );
        let print = |text: &str| read(text).to_string();
        // Printed as `in` however it was written; anything else as it is.
        assert_eq!(print("a = 1 or a = 2 or a = 3"), "a in (1, 2, 3)");
        // Branches wrapped alone in an AND or OR print as they would
        // bare: as `in`, so the text reads back the same (found by the
        // `queries` target, seed 123123).
        let wrapped = Condition::Any(vec![
            Condition::All(vec![Condition::eq("a", "s0")]),
            Condition::Any(vec![Condition::eq("a", 0)]),
        ]);
        assert_eq!(wrapped.to_string(), r#"a in ("s0", 0)"#);
        assert_eq!((!wrapped.clone()).to_string(), r#"a not in ("s0", 0)"#);
        let alone = Condition::All(vec![wrapped]);
        assert_eq!(alone.to_string(), r#"a in ("s0", 0)"#);
        for condition in [alone.clone(), !alone] {
            let text = condition.to_string();
            let back: Condition = text.parse().unwrap();
            assert_eq!(back.to_string(), text);
        }
        assert_eq!(print("not (a = 1 or a = 2)"), "a not in (1, 2)");
        assert_eq!(print("x = 0 and (a = 1 or a = 2)"), "x = 0 and a in (1, 2)");
        assert_eq!(print("a in (1, 2) or b = 3"), "a in (1, 2) or b = 3");
        assert_eq!(print("a = 1 or b = 2"), "a = 1 or b = 2");
        assert_eq!(print("a = 1 or a > 2"), "a = 1 or a > 2");
        assert_eq!(print("a in (1)"), "a = 1");
        assert_eq!(print("a in ()"), "false");
        assert_eq!(
            print(r#"tags[*] in ([1], {"b": null}, id("00000000-0000-0000-0000-000000000000"))"#),
            r#"tags[*] in ([1], {"b":null}, id("00000000-0000-0000-0000-000000000000"))"#
        );
        assert_eq!(
            error("a in 1"),
            "expected `(`, found `1` at line 1 column 6"
        );
        assert_eq!(
            error("a in (1,)"),
            "expected a value, found `)` at line 1 column 9"
        );
        assert_eq!(
            error("a not = 1"),
            "expected `in`, found `=` at line 1 column 7"
        );
    }

    #[test]
    fn the_places_kept_free_are_errors() {
        // SPEC §111.3: what a later version may give a meaning.
        for text in [
            "a = b",
            "a in (1, b)",
            "a = :b",
            ":a = 1",
            "a = $b",
            "$a = 1",
            "a = 1 group by b",
            "set a = 1",
        ] {
            error(text);
        }
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
            "expected a comparison (=, !=, <, <=, >, >=, contains, starts with, is null, in, has), found the end at line 1 column 2"
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

    /// Random aggregates of every shape (SPEC §157), their paths and
    /// names drawn also from the words the aggregate's text gives a
    /// meaning: each one's text reads back as an aggregate with the same
    /// text, which gives the same rows.
    #[test]
    fn every_aggregate_reads_back_from_its_text() {
        let mut rng = XorShift(0x5eed_2222);
        let docs: Vec<Document> = (0..40).map(|_| random_doc(&mut rng, 2)).collect();
        for round in 0..3000 {
            let aggregate = random_aggregate(&mut rng);
            let text = aggregate.to_string();
            let read: Aggregate = text
                .parse()
                .unwrap_or_else(|e| panic!("round {round}: {text}: {e}"));
            assert_eq!(read.to_string(), text, "round {round}");
            // Read twice, the same: a condition of one, `All([c])`, is
            // written as `c`, as for a filter.
            let again: Aggregate = read.to_string().parse().unwrap();
            assert_eq!(
                format!("{again:?}"),
                format!("{read:?}"),
                "round {round}: {text}"
            );
            let (want, got) = (aggregate.apply(docs.clone()), read.apply(docs.clone()));
            assert_eq!(
                format!("{got:?}"),
                format!("{want:?}"),
                "round {round}: {text}"
            );
        }
    }

    /// A path, or one of the aggregate's words, as a name.
    fn random_aggregate_path(rng: &mut XorShift) -> String {
        const WORDS: [&str; 12] = [
            "count", "group", "by", "having", "as", "filter", "window", "unwind", "distinct",
            "last", "order", "limit",
        ];
        match rng.below(3) {
            0 => WORDS[rng.below(WORDS.len())].to_string(),
            _ => random_path(rng),
        }
    }

    fn random_aggregate(rng: &mut XorShift) -> Aggregate {
        let mut aggregate = Aggregate::new();
        if rng.below(4) == 0 {
            let duration = [
                Duration::from_secs(300),
                Duration::from_millis(1500),
                Duration::from_nanos(7),
                Duration::from_secs(2 * 86_400),
                Duration::ZERO,
            ][rng.below(5)];
            aggregate = aggregate.window(random_aggregate_path(rng), duration);
        }
        for _ in 0..rng.below(3) {
            aggregate = aggregate.unwind(random_aggregate_path(rng));
        }
        for i in 0..rng.below(4) {
            let path = random_aggregate_path(rng);
            let column = match rng.below(8) {
                0 => aggregate.count(),
                1 => aggregate.sum(path),
                2 => aggregate.min(path),
                3 => aggregate.max(path),
                4 => aggregate.avg(path),
                5 => aggregate.count_distinct(path),
                6 => aggregate.first(path),
                _ => aggregate.last(path),
            };
            let column = match rng.below(3) {
                0 => column.filter(random_condition(rng, 2)),
                _ => column,
            };
            // A name of its own: two columns of one name are refused.
            let name = format!("{}{i}", random_aggregate_path(rng));
            aggregate = column.named(name);
        }
        if let Some(by) = (rng.below(3) == 0).then(|| random_aggregate_path(rng)) {
            // `by` on a `first` or `last` only, the last column added.
            if let Some(column) = aggregate.columns.last_mut()
                && matches!(column.kind, Kind::First | Kind::Last)
            {
                column.by = Some(by);
            }
        }
        for _ in 0..rng.below(3) {
            aggregate = aggregate.group_by(random_aggregate_path(rng));
        }
        if rng.below(3) == 0 {
            aggregate = aggregate.having(random_condition(rng, 2));
        }
        for _ in 0..rng.below(3) {
            aggregate = match rng.below(2) {
                0 => aggregate.then_asc(random_aggregate_path(rng)),
                _ => aggregate.then_desc(random_aggregate_path(rng)),
            };
        }
        if rng.below(3) == 0 {
            aggregate = aggregate.limit(rng.below(5));
        }
        if rng.below(3) == 0 {
            aggregate = aggregate.skip(rng.below(5));
        }
        aggregate
    }

    /// What the aggregate's text means, against the builders (SPEC §157):
    /// the doc's example, each column's spelling, a duration's units.
    #[test]
    fn an_aggregate_s_text_says_what_its_builders_say() {
        let read =
            |text: &str| -> Aggregate { text.parse().unwrap_or_else(|e| panic!("{text}: {e}")) };
        let built = Aggregate::new()
            .count()
            .named("n")
            .avg("hours")
            .named("mean")
            .count()
            .filter(Condition::eq("status", "open"))
            .named("open")
            .last("status")
            .by("changed")
            .named("latest")
            .group_by("team")
            .having(Condition::gt("n", 10))
            .sort_desc("n")
            .limit(5);
        let text = r#"count() as n, avg(hours) as mean,
            count() filter (where status = "open") as open,
            last(status order by changed) as latest
            group by team having n > 10 order by n desc limit 5"#;
        assert_eq!(format!("{:?}", read(text)), format!("{built:?}"));
        assert_eq!(
            built.to_string(),
            r#"count() as n, avg(hours) as mean, count() filter (where status = "open") as open, last(status order by changed) as latest group by team having n > 10 order by n desc limit 5"#
        );
        // `count(*)` is `count()`; `distinct` makes it `count_distinct`;
        // any case.
        assert_eq!(read("COUNT(*) AS n").to_string(), "count() as n");
        assert_eq!(
            format!("{:?}", read("count(distinct tags) as kinds")),
            format!(
                "{:?}",
                Aggregate::new().count_distinct("tags").named("kinds")
            )
        );
        let windowed = |text: &str| read(text).window.map(|w| (w.path, w.duration));
        assert_eq!(
            windowed("window at 5m"),
            Some(("at".into(), Duration::from_secs(300)))
        );
        assert_eq!(
            windowed("window at 90s").unwrap().1,
            Duration::from_secs(90)
        );
        assert_eq!(
            windowed("window at 250ms").unwrap().1,
            Duration::from_millis(250)
        );
        assert_eq!(
            windowed("window at 2d").unwrap().1,
            Duration::from_secs(172_800)
        );
        assert_eq!(read("window at 120m").to_string(), "window at 2h");
        assert_eq!(read("window at 90s").to_string(), "window at 90s");
        assert_eq!(read("window at 0ns").to_string(), "window at 0s");
        assert_eq!(
            read("unwind items, items.parts sum(items.parts.qty) as n group by items.sku")
                .to_string(),
            "unwind items, items.parts sum(items.parts.qty) as n group by items.sku"
        );
        assert_eq!(read("").to_string(), "");
        assert_eq!(read("group by a, b").group_by, ["a", "b"]);
        // Its words are paths and names where a clause can't start.
        let words = read("count() as having, sum(group) as as group by group having having > 1");
        assert_eq!(words.group_by, ["group"]);
        assert_eq!(
            (words
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()),
            ["having", "as"]
        );
        assert_eq!(words.columns[1].path.as_deref(), Some("group"));
        assert_eq!(words.rows.conditions.len(), 1);
        assert_eq!(read(&words.to_string()).to_string(), words.to_string());
    }

    /// An aggregate's text that isn't one, and what is said of it.
    #[test]
    fn an_aggregate_s_mistakes_are_named() {
        let error = |text: &str| match text.parse::<Aggregate>() {
            Ok(aggregate) => panic!("{text} parsed as {aggregate:?}"),
            Err(e) => e.to_string(),
        };
        for (text, says) in [
            (
                "count() n",
                "expected `as` and the column's name, found `n`",
            ),
            ("count() as", "expected a field"),
            ("sum() as n", "expected a field"),
            ("median(x) as m", "expected the end, found `median`"),
            ("count() as n,", "expected a column"),
            ("count() as n group status", "expected `by`"),
            ("count() as n having", "expected"),
            ("first(a order b) as f", "expected `by`"),
            ("sum(a order by b) as s", "expected `)`"),
            ("count() filter (status = 1) as n", "expected `where`"),
            ("window at 5", "expected a unit after the number"),
            ("window at 5min", "expected a unit after the number"),
            ("window at m", "expected a number"),
            ("window at 99999999999999999999d", "too large"),
            ("window at 999999999999999d", "the duration is too long"),
            ("count() as n limit x", "expected a number"),
            ("count(hours) as n", "count() takes no path"),
            ("group by a count() as n", "expected the end"),
        ] {
            let message = error(text);
            assert!(message.contains(says), "{text}: {message}");
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
            Op::StartsWith,
        ][rng.below(8)]
        .clone()
    }

    fn random_condition(rng: &mut XorShift, depth: usize) -> Condition {
        let kinds = if depth == 0 { 5 } else { 9 };
        let few = |rng: &mut XorShift| {
            (0..rng.below(4))
                .map(|_| random_condition(rng, depth - 1))
                .collect()
        };
        match rng.below(kinds) {
            0 | 1 => Condition::compare(random_path(rng), random_op(rng), random_value(rng, 1)),
            2 => Condition::exists(random_path(rng)),
            3 => Condition::size(random_path(rng), random_op(rng), rng.below(3)),
            4 => {
                let values: Vec<Document> =
                    (0..rng.below(4)).map(|_| random_value(rng, 1)).collect();
                Condition::is_in(random_path(rng), values)
            }
            5 => Condition::elem_match(random_path(rng), random_condition(rng, depth - 1)),
            6 => Condition::All(few(rng)),
            7 => Condition::Any(few(rng)),
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
        if rng.below(3) == 0 {
            filter = filter.except(
                (0..rng.below(3))
                    .map(|_| random_path(rng))
                    .collect::<Vec<_>>(),
            );
        }
        filter
    }
}
