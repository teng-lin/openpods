// SPDX-License-Identifier: AGPL-3.0-only
//! Identify test-only syntax; never classify production functions by their names.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use syn::{
    parse::Parser, punctuated::Punctuated, spanned::Spanned, visit::Visit, Attribute, Meta, Token,
};

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let values = Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .expect("valid cfg expression");
            if list.path.is_ident("all") {
                values.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                !values.is_empty() && values.iter().all(requires_test)
            } else {
                false
            }
        }
        _ => false,
    }
}
fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|part| part.ident == "test")
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<Meta>()
                    .is_ok_and(|meta| requires_test(&meta)))
    })
}
struct Collector<'a> {
    path: &'a Path,
    ranges: Vec<[usize; 2]>,
    external: Vec<PathBuf>,
}
impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if test_only(&item.attrs) {
            self.ranges.push([
                item.attrs
                    .first()
                    .map_or(item.span().start().line, |a| a.span().start().line),
                item.span().end().line,
            ]);
            if item.content.is_none() {
                let parent = self.path.parent().expect("source directory");
                let filename = format!("{}.rs", item.ident);
                let explicit = item.attrs.iter().find_map(|attr| {
                    if !attr.path().is_ident("path") {
                        return None;
                    }
                    match &attr.meta {
                        Meta::NameValue(value) => match &value.value {
                            syn::Expr::Lit(value) => match &value.lit {
                                syn::Lit::Str(path) => Some(parent.join(path.value())),
                                _ => None,
                            },
                            _ => None,
                        },
                        _ => None,
                    }
                });
                let direct = parent.join(&filename);
                let nested = parent.join(item.ident.to_string()).join("mod.rs");
                self.external.push(explicit.unwrap_or_else(|| {
                    if direct.exists() {
                        direct
                    } else {
                        nested
                    }
                }));
            }
        } else {
            syn::visit::visit_item_mod(self, item);
        }
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if test_only(&item.attrs) {
            self.ranges.push([
                item.attrs
                    .first()
                    .map_or(item.span().start().line, |a| a.span().start().line),
                item.span().end().line,
            ]);
        } else {
            syn::visit::visit_item_fn(self, item);
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let report: serde_json::Value = serde_json::from_slice(&fs::read(
        std::env::args()
            .nth(1)
            .ok_or("expected LLVM JSON report path")?,
    )?)?;
    let mut ranges = BTreeMap::<String, Vec<[usize; 2]>>::new();
    let mut external = Vec::new();
    for data in report["data"].as_array().ok_or("missing data")? {
        for file in data["files"].as_array().ok_or("missing files")? {
            let name = file["filename"].as_str().ok_or("missing filename")?;
            if !name.ends_with(".rs") {
                continue;
            }
            let path = Path::new(name);
            let source = fs::read_to_string(path)?;
            let syntax = syn::parse_file(&source)?;
            let mut collector = Collector {
                path,
                ranges: Vec::new(),
                external: Vec::new(),
            };
            collector.visit_file(&syntax);
            ranges.insert(name.to_owned(), collector.ranges);
            external.extend(collector.external);
        }
    }
    for path in external {
        let canonical = path.canonicalize()?;
        let name = canonical.to_str().ok_or("non-UTF8 path")?.to_owned();
        if ranges.contains_key(&name) {
            ranges.insert(
                name,
                vec![[1, fs::read_to_string(canonical)?.lines().count()]],
            );
        }
    }
    println!("{}", serde_json::to_string_pretty(&ranges)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conditional_production_code_is_never_removed() {
        for expression in [
            "test",
            "all(test, feature = \"ble\")",
            "any(test, all(test, unix))",
        ] {
            assert!(requires_test(
                &syn::parse_str::<Meta>(expression).expect("cfg")
            ));
        }
        for expression in [
            "unix",
            "not(test)",
            "any(test, feature = \"ble\")",
            "all(unix, feature = \"ble\")",
        ] {
            assert!(!requires_test(
                &syn::parse_str::<Meta>(expression).expect("cfg")
            ));
        }
    }
    #[test]
    fn spans_cover_only_test_functions_and_test_modules() {
        let source = "fn production() {}\n#[cfg(test)]\nmod checks { fn helper() {} }\n#[tokio::test]\nasync fn integration() {}\n#[cfg(any(test, feature = \"ble\"))]\nfn conditional_production() {}\n";
        let syntax = syn::parse_file(source).expect("syntax");
        let mut collector = Collector {
            path: Path::new("src/lib.rs"),
            ranges: vec![],
            external: vec![],
        };
        collector.visit_file(&syntax);
        assert_eq!(collector.ranges, vec![[2, 3], [4, 5]]);
    }
}
