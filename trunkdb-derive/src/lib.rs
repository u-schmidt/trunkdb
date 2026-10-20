//! `#[trunkdb::document]`, re-exported by trunkdb behind its `derive`
//! feature, where it's documented (SPEC §114).

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::Nothing;
use syn::{
    Error, GenericArgument, ItemStruct, PathArguments, PathSegment, Result, Type,
    parse_macro_input, parse_quote,
};

#[proc_macro_attribute]
pub fn document(attr: TokenStream, item: TokenStream) -> TokenStream {
    parse_macro_input!(attr as Nothing);
    let input = parse_macro_input!(item as ItemStruct);
    match expand(input) {
        Ok(output) => output.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(mut input: ItemStruct) -> Result<proc_macro2::TokenStream> {
    let mut found = false;
    for field in input.fields.iter_mut() {
        let Some(index) = field
            .attrs
            .iter()
            .position(|attr| attr.path().is_ident("id"))
        else {
            continue;
        };
        let attr = field.attrs.remove(index);
        attr.meta.require_path_only()?;
        if found {
            return Err(Error::new_spanned(attr, "only one field can be the id"));
        }
        if !is_id_type(&field.ty) {
            return Err(Error::new_spanned(
                &field.ty,
                "the id must be a `DocId` or an `Option<DocId>`",
            ));
        }
        field.attrs.push(parse_quote!(#[serde(rename = "_id")]));
        found = true;
    }
    Ok(quote!(#input))
}

fn is_id_type(ty: &Type) -> bool {
    if is_doc_id(ty) {
        return true;
    }
    let Some(segment) = last_segment(ty) else {
        return false;
    };
    if segment.ident != "Option" {
        return false;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return false;
    };
    match args.args.first() {
        Some(GenericArgument::Type(inner)) => args.args.len() == 1 && is_doc_id(inner),
        _ => false,
    }
}

fn is_doc_id(ty: &Type) -> bool {
    match last_segment(ty) {
        Some(segment) => segment.ident == "DocId" && segment.arguments.is_none(),
        None => false,
    }
}

fn last_segment(ty: &Type) -> Option<&PathSegment> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path.segments.last()
}
