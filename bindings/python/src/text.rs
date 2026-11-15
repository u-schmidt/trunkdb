//! trunkdb's texts, bound to a call's parameters (SPEC §163, §197.3): a
//! filter, an aggregate, an update's operators, as the C ABI takes them.

use crate::convert;
use crate::errors::Parse;
use db::Document;
use db::query::{Aggregate, BindError, Filter, Params, ParseFilterError, Template, Update};
use pyo3::prelude::*;
use pyo3::types::PyDict;

/// A filter's text bound to `params`: every value of them if `only` is
/// false, a value it has no parameter of refused; else only its own.
/// Empty is every document.
pub(crate) fn filter(py: Python<'_>, text: &str, params: &Params, only: bool) -> PyResult<Filter> {
    bound(py, "filter", text, params, only, Template::<Filter>::bind)
}

/// An aggregate's text, bound as `filter` binds.
pub(crate) fn aggregate(
    py: Python<'_>,
    text: &str,
    params: &Params,
    only: bool,
) -> PyResult<Aggregate> {
    bound(
        py,
        "aggregate",
        text,
        params,
        only,
        Template::<Aggregate>::bind,
    )
}

fn bound<T>(
    py: Python<'_>,
    what: &'static str,
    text: &str,
    params: &Params,
    only: bool,
    bind: fn(&Template<T>, &Params) -> Result<T, BindError>,
) -> PyResult<T>
where
    Template<T>: std::str::FromStr<Err = ParseFilterError>,
{
    let template: Template<T> = text.parse().map_err(|e: ParseFilterError| {
        let mut parse = Parse::new(what, &e);
        parse.text = Some(text.to_string());
        parse.offset = Some(e.offset());
        parse.into_err(py)
    })?;
    let mine;
    let params = if only {
        mine = params.only(&template.params());
        &mine
    } else {
        params
    };
    bind(&template, params).map_err(|e| {
        let mut parse = Parse::new(what, &e);
        parse.text = Some(text.to_string());
        parse.offset = e.offset();
        parse.parameter = Some(e.name().to_string());
        parse.into_err(py)
    })
}

/// An aggregate's filter and text bound to `params`, each taking its
/// own, a value neither has a parameter of refused (`Params::unused`).
pub(crate) fn aggregate_args(
    py: Python<'_>,
    filter_text: &str,
    aggregate_text: &str,
    params: &Params,
) -> PyResult<(Filter, Aggregate)> {
    let filter = filter(py, filter_text, params, true)?;
    let aggregate = aggregate(py, aggregate_text, params, true)?;
    let names = [
        names_in::<Filter>(filter_text),
        names_in::<Aggregate>(aggregate_text),
    ]
    .concat();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    if let Some(name) = params.unused(&names) {
        let mut parse = Parse::new(
            "params",
            format!("a value for :{name}, which no text of the call has a parameter of"),
        );
        parse.parameter = Some(name.to_string());
        return Err(parse.into_err(py));
    }
    Ok((filter, aggregate))
}

/// The names of a text's parameters; none if it doesn't read.
fn names_in<T>(text: &str) -> Vec<String>
where
    Template<T>: std::str::FromStr,
{
    match text.parse::<Template<T>>() {
        Ok(template) => template.params().iter().map(|n| n.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// An update's operators, a dict as MongoDB writes them: `{"$set":
/// {"done": True}}`.
pub(crate) fn update(value: &Bound<'_, PyAny>) -> PyResult<Update> {
    let py = value.py();
    if !value.is_instance_of::<PyDict>() {
        return Err(Parse::new(
            "update",
            r#"the operators are a dict, like {"$set": {"done": True}}"#,
        )
        .into_err(py));
    }
    let doc: Document = convert::document_in(value, "update")?;
    Update::try_from(doc).map_err(|e| Parse::new("update", e).into_err(py))
}
