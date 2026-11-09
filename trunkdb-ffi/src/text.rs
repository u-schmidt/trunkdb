//! Text in and out (SPEC §177.3, §179): UTF-8 with a zero byte at its
//! end, borrowed going in, owned by the caller coming out.

use crate::error::Fail;
use db::query::{Aggregate, Filter, Params, Template, Update};
use db::{DocId, Document};
use std::ffi::{CStr, CString, c_char};

/// `text` as a C string, any zero byte in it replaced: a message can
/// quote what it was given. JSON escapes its own.
pub(crate) fn c_string(text: String) -> CString {
    CString::new(text).unwrap_or_else(|e| {
        let text = String::from_utf8_lossy(&e.into_vec()).replace('\0', "\u{fffd}");
        CString::new(text).expect("no zero byte left")
    })
}

/// `text` handed to the caller, theirs to give back with
/// `trunkdb_string_free`.
pub(crate) fn owned(text: String) -> *mut c_char {
    c_string(text).into_raw()
}

/// The text at `pointer`, `what` naming it in an error.
///
/// # Safety
/// `pointer` is null or a zero-terminated string that lasts the call.
pub(crate) unsafe fn text<'a>(pointer: *const c_char, what: &str) -> Result<&'a str, Fail> {
    // SAFETY: by the caller.
    unsafe { optional(pointer, what) }?.ok_or_else(|| Fail::argument(format!("{what} is null")))
}

/// As `text`, with null for none.
///
/// # Safety
/// As for `text`.
pub(crate) unsafe fn optional<'a>(
    pointer: *const c_char,
    what: &str,
) -> Result<Option<&'a str>, Fail> {
    if pointer.is_null() {
        return Ok(None);
    }
    // SAFETY: by the caller.
    let bytes = unsafe { CStr::from_ptr(pointer) };
    match bytes.to_str() {
        Ok(text) => Ok(Some(text)),
        Err(e) => Err(Fail::argument(format!(
            "{what} is not UTF-8: a byte that can't be at {}",
            e.valid_up_to()
        ))),
    }
}

/// Values for a text's parameters, an object as JSON; null or empty
/// for none.
pub(crate) fn params(text: Option<&str>) -> Result<Params, Fail> {
    match text.map(str::trim) {
        None | Some("") => Ok(Params::new()),
        Some(text) => text.parse().map_err(|e| Fail::parse("params", e)),
    }
}

/// A filter's text bound to `params`: those it has parameters for, if
/// `only`, or else every one, a value it has no parameter of refused.
/// Null or empty is every document.
pub(crate) fn filter(text: Option<&str>, params: &Params, only: bool) -> Result<Filter, Fail> {
    bound(
        "filter",
        text.unwrap_or(""),
        params,
        only,
        Template::<Filter>::bind,
    )
}

/// An aggregate's text, bound as `filter` binds.
pub(crate) fn aggregate(text: &str, params: &Params, only: bool) -> Result<Aggregate, Fail> {
    bound("aggregate", text, params, only, Template::<Aggregate>::bind)
}

fn bound<T>(
    what: &str,
    text: &str,
    params: &Params,
    only: bool,
    bind: fn(&Template<T>, &Params) -> Result<T, db::query::BindError>,
) -> Result<T, Fail>
where
    Template<T>: std::str::FromStr<Err = db::query::ParseFilterError>,
{
    let template: Template<T> = text.parse().map_err(|e: db::query::ParseFilterError| {
        Fail::parse(what, &e).with("offset", e.offset())
    })?;
    let mine;
    let params = if only {
        mine = params.only(&template.params());
        &mine
    } else {
        params
    };
    bind(&template, params).map_err(|e| {
        let fail = Fail::parse(what, &e).with("parameter", e.name());
        match e.offset() {
            Some(offset) => fail.with("offset", offset),
            None => fail,
        }
    })
}

/// A value of `params` that neither of two texts has a parameter of.
pub(crate) fn unused(params: &Params, texts: &[&[&str]]) -> Result<(), Fail> {
    match params
        .names()
        .find(|name| !texts.iter().any(|names| names.contains(name)))
    {
        Some(name) => Err(Fail::parse(
            "params",
            format!("a value for :{name}, which no text of the call has a parameter of"),
        )
        .with("parameter", name)),
        None => Ok(()),
    }
}

/// The names of a text's parameters, as written; none if it doesn't read.
pub(crate) fn names_in<T>(text: &str) -> Vec<String>
where
    Template<T>: std::str::FromStr,
{
    match text.parse::<Template<T>>() {
        Ok(template) => names(&template),
        Err(_) => Vec::new(),
    }
}

fn names<T>(template: &Template<T>) -> Vec<String> {
    template
        .params()
        .iter()
        .map(|name| name.to_string())
        .collect()
}

/// An id, as its text.
pub(crate) fn id(text: &str) -> Result<DocId, Fail> {
    text.parse().map_err(|e| Fail::parse("id", e))
}

/// A document, as tagged JSON (SPEC §30).
pub(crate) fn document(text: &str) -> Result<Document, Fail> {
    text.parse().map_err(|e| Fail::parse("document", e))
}

/// An update, as MongoDB's operators (SPEC §178.1).
pub(crate) fn update(text: &str) -> Result<Update, Fail> {
    text.parse().map_err(|e| Fail::parse("update", e))
}

/// Options as a JSON object, each key one of `known`; null or empty for
/// none.
pub(crate) fn options(
    text: Option<&str>,
    known: &[&str],
) -> Result<serde_json::Map<String, serde_json::Value>, Fail> {
    let text = match text.map(str::trim) {
        None | Some("") => return Ok(serde_json::Map::new()),
        Some(text) => text,
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Fail::parse("options", e))?;
    let serde_json::Value::Object(options) = value else {
        return Err(Fail::parse(
            "options",
            "an object, like {\"read_only\": true}",
        ));
    };
    if let Some(key) = options.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(Fail::parse(
            "options",
            format!("`{key}` is not an option; one of {}", known.join(", ")),
        ));
    }
    Ok(options)
}

/// An option that is a whole number from 0.
pub(crate) fn count_option(
    options: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<u64>, Fail> {
    match options.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| Fail::parse("options", format!("`{key}` is a whole number from 0"))),
    }
}

/// An option that is true or false.
pub(crate) fn flag_option(
    options: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<bool, Fail> {
    match options.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| Fail::parse("options", format!("`{key}` is true or false"))),
    }
}
