//! The production module tree, read the way the compiler reads it: start at
//! a crate root, follow module-scope `mod x;` declarations (with `#[path]`), drop what a test
//! build alone compiles. Shared by the source rules in this directory.
//!
//! `cfg` predicates are three-valued. `test` is False (the production build
//! never has it); `not`, `all` and `any` combine as in logic; anything else
//! (`feature = "x"`, `debug_assertions`, `target_os`, ...) is Unknown, and
//! `not(Unknown)` stays Unknown. An item is dropped only when its predicate
//! is definitely False, so `#[cfg(not(feature = "x"))]`,
//! `#[cfg(not(debug_assertions))]` and `#[cfg(not(any(test, feature = "x")))]`
//! stay in, and `#[cfg(all(test, feature = "x"))]` and
//! `#[cfg(not(not(test)))]` go. Functions marked `#[test]`, `#[tokio::test]`,
//! `#[sqlx::test]` or `#[dbtest::test]` are dropped.
//!
//! Fails closed (panics, so the rule tests fail) on what it cannot read: a
//! block-local file module, a `mod x;` it cannot find, `include!`, a `mod` declared inside a macro
//! body, and `#[cfg_attr(.., path = ..)]` on a module. Statement-level `cfg`
//! is not pruned (rare; the rules over-report).
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::{Attribute, ImplItem, Item, Meta, TraitItem, ext::IdentExt, visit::Visit};

/// One parsed production file; `path` is relative to `src/`, with `/`.
pub(super) struct Source {
    pub path: String,
    pub module: String,
    pub file: syn::File,
    /// Part of the binary crate (`main.rs` and below), not the library: the
    /// library cannot call into it.
    pub bin: bool,
}

pub(super) fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every production file reachable from `entry` (`lib.rs`, `main.rs`).
pub(super) fn production(entry: &str) -> Vec<Source> {
    let src = src_dir();
    let read = |rel: &Path| std::fs::read_to_string(src.join(rel)).ok();
    load_tree(&read, entry, entry == "main.rs")
}

/// [`production`] over any file source (the tests use a map).
pub(super) fn load_tree(
    read: &dyn Fn(&Path) -> Option<String>,
    entry: &str,
    bin: bool,
) -> Vec<Source> {
    let mut out = Vec::new();
    let entry = Path::new(entry);
    let module = entry
        .with_extension("")
        .to_string_lossy()
        .replace('/', "::");
    let crate_root = matches!(
        entry.file_name().and_then(|n| n.to_str()),
        Some("lib.rs" | "main.rs")
    );
    load(read, entry, module, bin, crate_root, &mut out);
    out
}

fn load(
    read: &dyn Fn(&Path) -> Option<String>,
    rel: &Path,
    module: String,
    bin: bool,
    children_beside_file: bool,
    out: &mut Vec<Source>,
) {
    let text = read(rel).unwrap_or_else(|| panic!("{}: cannot read", rel.display()));
    let Some(file) = production_items(rel, &text, |_| true) else {
        return;
    };
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let children = if children_beside_file || name == "mod.rs" {
        parent.to_path_buf()
    } else {
        parent.join(rel.file_stem().unwrap_or_default())
    };
    let mut files = Vec::new();
    declared(
        &file.items,
        parent,
        &children,
        &module,
        read,
        rel,
        &mut files,
    );
    out.push(Source {
        path: rel.to_string_lossy().replace('\\', "/"),
        module,
        file,
        bin,
    });
    for (child, module, explicit) in files {
        load(read, &child, module, bin, explicit, out);
    }
}

/// Parse a production file and apply the reader's checks to the selected top-level items.
pub(super) fn production_items(
    rel: &Path,
    text: &str,
    keep: impl FnMut(&Item) -> bool,
) -> Option<syn::File> {
    let mut file = syn::parse_file(text).unwrap_or_else(|e| panic!("{}: {e}", rel.display()));
    if cfg_excludes(&file.attrs) {
        return None;
    }
    file.items.retain(keep);
    prune(&mut file.items);
    Unreadable {
        from: rel,
        in_block: false,
    }
    .visit_file(&file);
    Some(file)
}

/// Constructs that add code the scan would not read.
struct Unreadable<'a> {
    from: &'a Path,
    in_block: bool,
}

impl<'ast> Visit<'ast> for Unreadable<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        // File resolution only walks file and inline-module item lists.
        // Keep the block context through inline modules and nested items.
        syn::visit::visit_block(
            &mut Self {
                from: self.from,
                in_block: true,
            },
            block,
        );
    }

    fn visit_item(&mut self, item: &'ast Item) {
        // prune() has already handled module-scope items. Within a block,
        // only a built-in cfg predicate can establish this item is absent.
        if !self.in_block || !cfg_excludes(item_attrs(item)) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if !cfg_excludes(&variant.attrs) {
            syn::visit::visit_variant(self, variant);
        }
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        if !cfg_excludes(&field.attrs) {
            syn::visit::visit_field(self, field);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !self.in_block || !cfg_excludes(impl_item_attrs(item)) {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if !self.in_block || !cfg_excludes(trait_item_attrs(item)) {
            syn::visit::visit_trait_item(self, item);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident.unraw() == "include")
        {
            panic!(
                "{}: include! adds code the source rules cannot read",
                self.from.display()
            );
        }
        // A `mod` keyword followed by a `;` before any `{` body, however the
        // name is spelled (`mod hidden;`, `mod $name;`, `pub(crate) mod $n;`).
        let tokens = flat(mac.tokens.clone());
        for (at, token) in tokens.iter().enumerate() {
            if !matches!(token, TokenTree::Ident(m) if m == "mod") {
                continue;
            }
            for next in tokens.iter().skip(at + 1).take(4) {
                match next {
                    TokenTree::Punct(p) if p.as_char() == ';' => panic!(
                        "{}: a `mod` declared inside a macro adds a file the source rules cannot follow",
                        self.from.display()
                    ),
                    TokenTree::Group(g) if g.delimiter() == proc_macro2::Delimiter::Brace => break,
                    _ => {}
                }
            }
        }
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if self.in_block && module.content.is_none() {
            panic!(
                "{}: block-local file module `{}` is unsupported by the source rules",
                self.from.display(),
                module.ident
            );
        }
        for attr in &module.attrs {
            if attr
                .path()
                .get_ident()
                .is_some_and(|id| id.unraw() == "cfg_attr")
                && let Meta::List(list) = &attr.meta
                && flat(list.tokens.clone())
                    .iter()
                    .any(|t| matches!(t, TokenTree::Ident(i) if i.unraw() == "path"))
            {
                panic!(
                    "{}: cfg_attr(.., path = ..) on a module chooses a file the source rules cannot predict",
                    self.from.display()
                );
            }
        }
        syn::visit::visit_item_mod(self, module);
    }
}

/// File modules declared by `items`; `dir` is where a plain `mod x;` looks,
/// `base` is what a `#[path]` is relative to.
fn declared(
    items: &[Item],
    base: &Path,
    dir: &Path,
    parent_module: &str,
    read: &dyn Fn(&Path) -> Option<String>,
    from: &Path,
    out: &mut Vec<(PathBuf, String, bool)>,
) {
    for item in items {
        let Item::Mod(module) = item else { continue };
        let ident = module.ident.unraw().to_string();
        let module_path = format!("{parent_module}::{ident}");
        let explicit = module.attrs.iter().find_map(|a| match &a.meta {
            Meta::NameValue(v) if v.path.get_ident().is_some_and(|i| i.unraw() == "path") => {
                match &v.value {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) => Some(s.value()),
                    _ => None,
                }
            }
            _ => None,
        });
        if let Some((_, inner)) = &module.content {
            let child_dir = match explicit {
                Some(path) => base.join(path),
                None => dir.join(&ident),
            };
            declared(inner, &child_dir, &child_dir, &module_path, read, from, out);
            continue;
        }
        let found = if let Some(path) = &explicit {
            Some(base.join(path))
        } else {
            [
                dir.join(format!("{ident}.rs")),
                dir.join(&ident).join("mod.rs"),
            ]
            .into_iter()
            .find(|candidate| read(candidate).is_some())
        };
        match found {
            Some(path) if read(&path).is_some() => {
                out.push((path, module_path, explicit.is_some()))
            }
            _ => panic!(
                "{}: cannot resolve `mod {ident};` (the source rules fail closed)",
                from.display()
            ),
        }
    }
}

/// A `cfg` predicate in a build without `cfg(test)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn negate(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

fn cfg_holds(meta: &Meta) -> Tri {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Tri::False,
        Meta::List(list) => {
            let name = list
                .path
                .get_ident()
                .map(|i| i.unraw().to_string())
                .unwrap_or_default();
            let Ok(args) = list.parse_args_with(
                syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return Tri::Unknown;
            };
            let values: Vec<Tri> = args.iter().map(cfg_holds).collect();
            match name.as_str() {
                "not" if values.len() == 1 => values[0].negate(),
                "all" if values.contains(&Tri::False) => Tri::False,
                "all" if values.iter().all(|v| *v == Tri::True) => Tri::True,
                "any" if values.contains(&Tri::True) => Tri::True,
                "any" if values.iter().all(|v| *v == Tri::False) => Tri::False,
                _ => Tri::Unknown,
            }
        }
        _ => Tri::Unknown,
    }
}

fn cfg_excludes(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| cfg_holds(&meta) == Tri::False)
    })
}

pub(super) fn possible_in_production(predicate: &Meta) -> bool {
    cfg_holds(predicate) != Tri::False
}

pub(super) fn kept(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        let last = attr
            .path()
            .segments
            .last()
            .map(|s| s.ident.unraw().to_string());
        match (&attr.meta, last.as_deref()) {
            (Meta::List(list), Some("cfg")) => list
                .parse_args::<Meta>()
                .map(|m| possible_in_production(&m))
                .unwrap_or(true),
            (_, Some("test")) => false,
            _ => true,
        }
    })
}

pub(super) fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(c) => &c.attrs,
        ImplItem::Fn(f) => &f.attrs,
        ImplItem::Type(t) => &t.attrs,
        ImplItem::Macro(m) => &m.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(c) => &c.attrs,
        TraitItem::Fn(f) => &f.attrs,
        TraitItem::Type(t) => &t.attrs,
        TraitItem::Macro(m) => &m.attrs,
        _ => &[],
    }
}

fn prune(items: &mut Vec<Item>) {
    items.retain(|item| kept(item_attrs(item)));
    for item in items.iter_mut() {
        match item {
            Item::Mod(module) => {
                if let Some((_, inner)) = &mut module.content {
                    prune(inner);
                }
            }
            Item::Impl(imp) => imp.items.retain(|i| kept(impl_item_attrs(i))),
            Item::Trait(tr) => tr.items.retain(|i| kept(trait_item_attrs(i))),
            _ => {}
        }
    }
}

/// Match both interpretations of capital runs: APIkey contains "apikey",
/// while clientAPIKey contains the adjacent words "api" and "key".
pub(super) fn word_forms(ident: &str) -> impl Iterator<Item = Vec<String>> {
    [false, true]
        .into_iter()
        .map(move |split_acronyms| words(ident, split_acronyms))
}

fn words(ident: &str, split_acronyms: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut chars = ident.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            previous = None;
            continue;
        }
        let starts_word = previous.is_some_and(|p| {
            p.is_lowercase()
                || p.is_ascii_digit()
                || (split_acronyms
                    && p.is_uppercase()
                    && chars.peek().is_some_and(|next| next.is_lowercase()))
        });
        if c.is_uppercase() && starts_word {
            out.push(std::mem::take(&mut word));
        }
        previous = Some(c);
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// The enclosing modules, impls and functions of the node being visited.
#[derive(Default)]
pub(super) struct Scope(Vec<String>);

impl Scope {
    pub fn enter(&mut self, name: String) {
        self.0.push(name);
    }
    pub fn leave(&mut self) {
        self.0.pop();
    }
    /// `outer::inner`, or `<file>` outside any named item.
    pub fn path(&self) -> String {
        if self.0.is_empty() {
            "<file>".to_owned()
        } else {
            self.0.join("::")
        }
    }
}

/// The last identifier of an impl's self type (`impl Foo<T>` is `Foo`).
pub(super) fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.unraw().to_string())
            .unwrap_or_default(),
        syn::Type::Reference(r) => type_name(&r.elem),
        syn::Type::Paren(p) => type_name(&p.elem),
        syn::Type::Group(g) => type_name(&g.elem),
        _ => "<type>".to_owned(),
    }
}

/// Adds the `visit_*` methods that keep a [`Scope`] in a field called
/// `scope` for a `syn::visit::Visit` impl.
macro_rules! track_scope {
    () => {
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.ident).to_string());
            syn::visit::visit_item_mod(self, i);
            self.scope.leave();
        }
        fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
            self.scope.enter(format!(
                "impl {}",
                $crate::source_rules::tree::type_name(&i.self_ty)
            ));
            syn::visit::visit_item_impl(self, i);
            self.scope.leave();
        }
        fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.ident).to_string());
            syn::visit::visit_item_const(self, i);
            self.scope.leave();
        }
        fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.ident).to_string());
            syn::visit::visit_item_static(self, i);
            self.scope.leave();
        }
        fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
            self.scope
                .enter(format!("trait {}", syn::ext::IdentExt::unraw(&i.ident)));
            syn::visit::visit_item_trait(self, i);
            self.scope.leave();
        }
        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_item_fn(self, i);
            self.scope.leave();
        }
        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_impl_item_fn(self, i);
            self.scope.leave();
        }
        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_trait_item_fn(self, i);
            self.scope.leave();
        }
    };
}

/// Every token of a macro body, groups flattened, with the token after it.
pub(super) fn flat(tokens: TokenStream) -> Vec<TokenTree> {
    let mut out = Vec::new();
    for token in tokens {
        if let TokenTree::Group(group) = &token {
            let inner = flat(group.stream());
            out.push(token.clone());
            out.extend(inner);
        } else {
            out.push(token);
        }
    }
    out
}

/// Parses `source` as a file and prunes it like [`production`] does; for the
/// planted-violation fixtures.
pub(super) fn parse_fixture(path: &str, source: &str) -> Source {
    let mut file =
        syn::parse_file(source).unwrap_or_else(|e| panic!("fixture {path} does not parse: {e}"));
    prune(&mut file.items);
    let bin = path == "main.rs" || path == "commands.rs" || path.starts_with("commands/");
    Source {
        path: path.to_owned(),
        module: path.trim_end_matches(".rs").replace('/', "::"),
        file,
        bin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree(files: &[(&str, &str)], entry: &str) -> Vec<String> {
        let map: HashMap<PathBuf, String> = files
            .iter()
            .map(|(p, c)| (PathBuf::from(p), (*c).to_owned()))
            .collect();
        let read = move |p: &Path| map.get(p).cloned();
        let mut paths: Vec<String> = load_tree(&read, entry, false)
            .into_iter()
            .map(|s| s.path)
            .collect();
        paths.sort();
        paths
    }

    fn names(code: &str) -> Vec<String> {
        parse_fixture("x.rs", code)
            .file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(f) => Some(f.sig.ident.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn modules_resolve_like_the_compiler_does() {
        let found = tree(
            &[
                (
                    "lib.rs",
                    "mod a; mod b { mod c; } mod e; #[path = \"x/y.rs\"] mod z;",
                ),
                ("a.rs", "mod d;"),
                ("a/d.rs", ""),
                ("b/c.rs", ""),
                ("e/mod.rs", "mod f;"),
                ("e/f.rs", ""),
                ("x/y.rs", ""),
            ],
            "lib.rs",
        );
        assert_eq!(
            found,
            [
                "a.rs", "a/d.rs", "b/c.rs", "e/f.rs", "e/mod.rs", "lib.rs", "x/y.rs"
            ]
        );
    }

    #[test]
    fn inline_path_selects_the_directory_for_child_files() {
        let files = [
            ("lib.rs", "mod outer;"),
            (
                "outer.rs",
                r#"#[path = "chosen"] mod inline { mod child; }"#,
            ),
            ("outer/inline/child.rs", ""),
            ("chosen/child.rs", ""),
        ];
        assert_eq!(
            tree(&files, "lib.rs"),
            ["chosen/child.rs", "lib.rs", "outer.rs"]
        );
        assert_loader_refuses(
            || tree(&files[..3], "lib.rs"),
            "cannot resolve `mod child;`",
        );
    }

    #[test]
    fn inline_modules_in_explicit_files_start_beside_the_file() {
        let found = tree(
            &[
                ("lib.rs", r#"#[r#path = "custom/outer.rs"] mod outer;"#),
                (
                    "custom/outer.rs",
                    r#"mod inline { #[path = "marker.rs"] mod marker; mod child; }"#,
                ),
                ("custom/inline/marker.rs", ""),
                ("custom/inline/child.rs", ""),
            ],
            "lib.rs",
        );
        assert_eq!(
            found,
            [
                "custom/inline/child.rs",
                "custom/inline/marker.rs",
                "custom/outer.rs",
                "lib.rs"
            ]
        );
    }

    fn assert_loader_refuses(
        load: impl FnOnce() -> Vec<String> + std::panic::UnwindSafe,
        expected: &str,
    ) {
        let refused = std::panic::catch_unwind(load).expect_err("expected loader refusal");
        let message = refused.downcast_ref::<String>().expect("loader diagnostic");
        assert!(message.contains(expected), "{message}");
    }

    #[test]
    fn block_local_file_modules_fail_closed() {
        for source in [
            r#"fn f() { #[path = "other.rs"] mod child; }"#,
            r#"impl S { fn f() { mod inner { #[path = "other.rs"] mod child; } } }"#,
            r#"trait T { const C: () = { #[cfg(unix)] #[path = "other.rs"] mod child; }; }"#,
        ] {
            assert_loader_refuses(
                || {
                    tree(
                        &[("lib.rs", source), ("other.rs", "const _: () = ();")],
                        "lib.rs",
                    )
                },
                "lib.rs: block-local file module `child`",
            );
        }
    }

    #[test]
    fn block_local_inline_and_test_only_modules_are_allowed() {
        let source = r#"
            fn f() {
                mod local { pub fn helper() {} }
                #[cfg(all(test, unix))] #[path = "absent.rs"] mod child;
            }
            #[test] fn fixture() { #[path = "absent.rs"] mod child; }
            impl S { #[cfg(test)] fn helper() { #[path = "absent.rs"] mod child; } }
        "#;
        assert_eq!(tree(&[("lib.rs", source)], "lib.rs"), ["lib.rs"]);
    }

    #[test]
    fn local_associated_members_respect_their_cfg() {
        for member in [
            "impl S { CFG fn method() { MOD } }",
            "impl S { CFG const C: () = { MOD }; }",
            "trait T { CFG fn method() { MOD } }",
            "trait T { CFG const C: () = { MOD }; }",
        ] {
            let local = format!("fn f() {{ struct S; {member} }}")
                .replace("MOD", r#"#[path = "child.rs"] mod child;"#);
            let load = |cfg| {
                tree(
                    &[("lib.rs", &local.replace("CFG", cfg)), ("child.rs", "")],
                    "lib.rs",
                )
            };
            assert_eq!(load("#[cfg(all(test, unix))]"), ["lib.rs"], "{member}");
            for cfg in ["#[cfg(unix)]", "#[cfg(not(test))]"] {
                assert_loader_refuses(|| load(cfg), "lib.rs: block-local file module `child`");
            }
        }
    }

    #[test]
    fn variant_and_field_cfg_guard_const_blocks() {
        for declaration in [
            "enum E { A, CFG B = { MOD 1 } }",
            "struct S { CFG a: [u8; { MOD 1 }] }",
        ] {
            let load = |cfg: &str| {
                let source = declaration
                    .replace("CFG", cfg)
                    .replace("MOD", r#"#[path = "child.rs"] mod child;"#);
                tree(&[("lib.rs", &source), ("child.rs", "")], "lib.rs")
            };
            assert_eq!(load("#[cfg(test)]"), ["lib.rs"]);
            assert_loader_refuses(
                || load("#[cfg(unix)]"),
                "lib.rs: block-local file module `child`",
            );
        }
    }

    #[test]
    fn crate_root_respects_its_inner_cfg() {
        let load = |cfg: &str| {
            let root = format!("#![cfg({cfg})] mod child;");
            tree(&[("lib.rs", &root), ("child.rs", "")], "lib.rs")
        };
        assert!(load("test").is_empty());
        assert_eq!(load("unix"), ["child.rs", "lib.rs"]);
    }

    #[test]
    fn child_files_respect_their_inner_cfg() {
        let load = |cfg| {
            let child = format!(r#"#![cfg({cfg})] fn f() {{ #[path = "other.rs"] mod other; }}"#);
            tree(
                &[
                    ("lib.rs", "mod child;"),
                    ("child.rs", &child),
                    ("other.rs", ""),
                ],
                "lib.rs",
            )
        };
        assert_eq!(load("all(test, unix)"), ["lib.rs"]);
        for cfg in ["unix", "not(test)"] {
            assert_loader_refuses(|| load(cfg), "child.rs: block-local file module `other`");
        }
    }

    #[test]
    fn test_only_modules_are_not_followed() {
        let found = tree(
            &[(
                "lib.rs",
                "#[cfg(test)] mod t; #[cfg(all(test, feature = \"x\"))] mod u;",
            )],
            "lib.rs",
        );
        assert_eq!(found, ["lib.rs"]);
    }

    #[test]
    #[should_panic(expected = "cannot resolve `mod missing;`")]
    fn an_unresolvable_module_fails_closed() {
        tree(&[("lib.rs", "mod missing;")], "lib.rs");
    }

    #[test]
    #[should_panic(expected = "cannot resolve `mod missing;`")]
    fn a_path_to_a_missing_file_fails_closed() {
        tree(
            &[("lib.rs", "#[path = \"nope.rs\"] mod missing;")],
            "lib.rs",
        );
    }

    #[test]
    #[should_panic(expected = "include!")]
    fn include_fails_closed() {
        tree(&[("lib.rs", "include!(\"other.rs\");")], "lib.rs");
    }

    #[test]
    #[should_panic(expected = "include!")]
    fn include_inside_a_function_fails_closed() {
        tree(
            &[("lib.rs", "fn f() { include!(\"other.rs\"); }")],
            "lib.rs",
        );
    }

    #[test]
    #[should_panic(expected = "inside a macro")]
    fn a_mod_declared_in_a_macro_fails_closed() {
        tree(
            &[("lib.rs", "macro_rules! m { () => { mod hidden; } }")],
            "lib.rs",
        );
    }

    #[test]
    #[should_panic(expected = "inside a macro")]
    fn a_mod_named_by_a_macro_variable_fails_closed() {
        tree(
            &[(
                "lib.rs",
                "macro_rules! m { ($n:ident) => { pub(crate) mod $n; } }",
            )],
            "lib.rs",
        );
    }

    #[test]
    fn an_inline_mod_with_a_body_in_a_macro_is_fine() {
        let found = tree(
            &[(
                "lib.rs",
                "macro_rules! m { ($n:ident) => { mod $n { fn f() {} } }; () => { mod x { fn g() {} } } }",
            )],
            "lib.rs",
        );
        assert_eq!(found, ["lib.rs"]);
    }

    #[test]
    #[should_panic(expected = "cfg_attr(.., path")]
    fn cfg_attr_path_on_a_module_fails_closed() {
        tree(
            &[
                ("lib.rs", "#[cfg_attr(unix, path = \"unix.rs\")] mod m;"),
                ("m.rs", ""),
            ],
            "lib.rs",
        );
    }

    #[test]
    fn raw_include_is_rejected() {
        assert_loader_refuses(
            || tree(&[("lib.rs", r#"r#include!("other.rs");"#)], "lib.rs"),
            "lib.rs: include!",
        );
    }

    #[test]
    fn conditional_module_paths_with_raw_identifiers_are_rejected() {
        for attr in [
            r#"#[cfg_attr(all(), r#path = "chosen.rs")]"#,
            r#"#[r#cfg_attr(all(), path = "chosen.rs")]"#,
        ] {
            let source = format!("{attr} mod m;");
            assert_loader_refuses(
                || {
                    tree(
                        &[("lib.rs", &source), ("m.rs", ""), ("chosen.rs", "")],
                        "lib.rs",
                    )
                },
                "lib.rs: cfg_attr(.., path",
            );
        }
    }

    #[test]
    fn only_a_definitely_false_cfg_prunes() {
        let code = r#"
            #[cfg(test)] fn t() {}
            #[cfg(all(test, feature = "x"))] fn all_test_feature() {}
            #[cfg(not(not(test)))] fn double_not() {}
            #[cfg(any(test, test))] fn any_tests() {}
            #[cfg(not(feature = "x"))] fn not_feature() {}
            #[cfg(not(debug_assertions))] fn not_debug() {}
            #[cfg(feature = "x")] fn feature() {}
            #[cfg(not(test))] fn not_test() {}
            #[cfg(not(any(test, feature = "integration-tests")))] fn not_any() {}
            #[cfg(any(test, feature = "x"))] fn any_feature() {}
            #[cfg(all(unix, not(test)))] fn all_unix() {}
            #[cfg(all(unix, test))] fn unix_test() {}
            #[test] fn plain_test() {}
            #[tokio::test] async fn tokio_test() {}
            #[sqlx::test] async fn sqlx_test() {}
            #[dbtest::test] async fn dbtest_test() {}
            fn plain() {}
        "#;
        assert_eq!(
            names(code),
            [
                "not_feature",
                "not_debug",
                "feature",
                "not_test",
                "not_any",
                "any_feature",
                "all_unix",
                "plain"
            ]
        );
    }

    #[test]
    fn impl_and_trait_members_are_pruned_like_items() {
        let source = parse_fixture(
            "x.rs",
            r#"
            struct S;
            impl S {
                fn a(&self) {}
                #[cfg(test)] fn b(&self) {}
                #[cfg(not(feature = "x"))] fn c(&self) {}
            }
            trait T {
                fn a(&self) {}
                #[cfg(test)] fn b(&self) {}
                #[cfg(not(debug_assertions))] fn c(&self) {}
            }
            "#,
        );
        let mut impl_fns = Vec::new();
        let mut trait_fns = Vec::new();
        for item in &source.file.items {
            match item {
                Item::Impl(i) => impl_fns.extend(i.items.iter().filter_map(|m| match m {
                    ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
                    _ => None,
                })),
                Item::Trait(t) => trait_fns.extend(t.items.iter().filter_map(|m| match m {
                    TraitItem::Fn(f) => Some(f.sig.ident.to_string()),
                    _ => None,
                })),
                _ => {}
            }
        }
        assert_eq!(impl_fns, ["a", "c"]);
        assert_eq!(trait_fns, ["a", "c"]);
    }

    #[test]
    fn nested_test_modules_are_pruned() {
        let source = parse_fixture(
            "x.rs",
            "mod m { fn keep() {} #[cfg(test)] mod tests { fn gone() {} } }",
        );
        let Item::Mod(m) = &source.file.items[0] else {
            panic!("no module")
        };
        let inner = &m.content.as_ref().unwrap().1;
        assert_eq!(inner.len(), 1);
    }
}
