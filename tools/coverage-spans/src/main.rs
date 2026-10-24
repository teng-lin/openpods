// SPDX-License-Identifier: AGPL-3.0-only
//! Identify test-only syntax; never classify production functions by their names.
use std::{
    collections::{BTreeMap, BTreeSet},
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
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs: &[Attribute] = match item {
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Enum(item) => &item.attrs,
            syn::Item::ExternCrate(item) => &item.attrs,
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::ForeignMod(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Macro(item) => &item.attrs,
            syn::Item::Mod(_) => {
                syn::visit::visit_item(self, item);
                return;
            }
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Struct(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::TraitAlias(item) => &item.attrs,
            syn::Item::Type(item) => &item.attrs,
            syn::Item::Union(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            _ => &[],
        };
        if test_only(attrs) {
            self.ranges.push([
                attrs
                    .first()
                    .map_or(item.span().start().line, |a| a.span().start().line),
                item.span().end().line,
            ]);
        } else {
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            self.ranges.push([
                item.attrs
                    .first()
                    .map_or(item.span().start().line, |a| a.span().start().line),
                item.span().end().line,
            ]);
        } else {
            syn::visit::visit_impl_item_fn(self, item);
        }
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if test_only(&item.attrs) {
            self.ranges.push([
                item.attrs
                    .first()
                    .map_or(item.span().start().line, |a| a.span().start().line),
                item.span().end().line,
            ]);
        } else {
            syn::visit::visit_trait_item_fn(self, item);
        }
    }
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
fn collect_sources(root: &Path, paths: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_sources(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.insert(path);
        }
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let report: serde_json::Value = serde_json::from_slice(&fs::read(
        std::env::args()
            .nth(1)
            .ok_or("expected LLVM JSON report path")?,
    )?)?;
    let mut paths = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for data in report["data"].as_array().ok_or("missing data")? {
        for file in data["files"].as_array().ok_or("missing files")? {
            let name = file["filename"].as_str().ok_or("missing filename")?;
            if !name.ends_with(".rs") {
                continue;
            }
            let path = PathBuf::from(name);
            if let Some(root) = path
                .ancestors()
                .find(|p| p.file_name().is_some_and(|n| n == "src"))
            {
                roots.insert(root.to_owned());
            }
            paths.insert(path);
        }
    }
    // Module-only lib.rs files may have no instrumented lines, so LLVM omits
    // them. Parse the source roots too: their cfg(test) declarations establish
    // that external helpers such as test_bus.rs contain no production code.
    for root in roots {
        collect_sources(&root, &mut paths)?;
    }
    let mut ranges = BTreeMap::<String, Vec<[usize; 2]>>::new();
    let mut external = Vec::new();
    for path in paths {
        let source = fs::read_to_string(&path)?;
        let syntax = syn::parse_file(&source)?;
        let mut collector = Collector {
            path: &path,
            ranges: Vec::new(),
            external: Vec::new(),
        };
        collector.visit_file(&syntax);
        ranges.insert(
            path.to_str().ok_or("non-UTF8 path")?.to_owned(),
            if test_only(&syntax.attrs) {
                vec![[1, source.lines().count()]]
            } else {
                collector.ranges
            },
        );
        external.extend(collector.external);
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
    #[test]
    fn test_only_impls_and_methods_are_excluded_without_dropping_production_methods() {
        let syntax = syn::parse_file("impl Thing {\n#[cfg(test)]\nfn helper() {}\nfn production() {}\n}\n#[cfg(test)]\nimpl Fake { fn helper() {} }\n").expect("syntax");
        let mut collector = Collector {
            path: Path::new("src/lib.rs"),
            ranges: vec![],
            external: vec![],
        };
        collector.visit_file(&syntax);
        assert_eq!(collector.ranges, vec![[2, 3], [6, 7]]);
    }
}
