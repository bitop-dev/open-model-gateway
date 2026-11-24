//! Direct sources of operational event fields and Prometheus family labels.
//! Keep literals intact: spaces inside a fixed label are part of its value.
use std::collections::BTreeSet;

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::{self, Visit};

use super::tree::{Scope, Source, flat, parse_fixture, production};

type Reads = BTreeSet<(String, String)>;

struct Finder<'a> {
    path: &'a str,
    scope: Scope,
    entries: Vec<String>,
    reads: Vec<Reads>,
    // Local patterns and their initializers in the metric implementation.
    bindings: Vec<(String, BTreeSet<String>, String)>,
}

// One physical line per entry, including when a literal spans source lines.
fn token_text(value: &impl ToTokens) -> String {
    without_list_commas(value.to_token_stream())
        .to_string()
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Keywords after which a parenthesised group is a value or a type, not a
/// list: `return (a,)` is a tuple, `return (a)` is not.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "try", "type", "unsafe", "use",
    "where", "while", "yield",
];

/// rustfmt adds or drops the trailing comma of a list when it reflows it,
/// which changes no meaning in a bracket or brace group, nor in the argument
/// list of a call or tuple struct (a parenthesised group after a name that is
/// not a keyword): there an entry ignores it. It is kept everywhere else,
/// where it can change meaning: a parenthesised one-element tuple (`(a,)` is
/// not `(a)`) and anything inside a macro invocation (a macro may match it).
fn without_list_commas(stream: TokenStream) -> TokenStream {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut out = Vec::with_capacity(tokens.len());
    for (at, token) in tokens.iter().enumerate() {
        let TokenTree::Group(group) = token else {
            out.push(token.clone());
            continue;
        };
        let before = &tokens[..at];
        if matches!(before, [.., TokenTree::Punct(bang)] if bang.as_char() == '!') {
            out.push(token.clone());
            continue;
        }
        let list = match group.delimiter() {
            Delimiter::Bracket | Delimiter::Brace => true,
            Delimiter::Parenthesis => matches!(
                before,
                [.., TokenTree::Ident(name)] if !KEYWORDS.contains(&name.to_string().as_str())
            ),
            Delimiter::None => false,
        };
        let mut inner: Vec<TokenTree> = without_list_commas(group.stream()).into_iter().collect();
        if list && matches!(inner.last(), Some(TokenTree::Punct(comma)) if comma.as_char() == ',') {
            inner.pop();
        }
        let mut rebuilt = Group::new(group.delimiter(), inner.into_iter().collect());
        rebuilt.set_span(group.span());
        out.push(TokenTree::Group(rebuilt));
    }
    out.into_iter().collect()
}

/// Calls that give a registry constant labels or register a collector that
/// encodes its own (prometheus-client `Registry`).
const CONSTANT_LABELS: &[&str] = &[
    "with_labels",
    "with_prefix_and_labels",
    "sub_registry_with_label",
    "sub_registry_with_labels",
    "register_collector",
];

/// A path naming a method of `Metrics` (`Metrics::observe_cache`).
fn metrics_method_path(path: &syn::Path) -> bool {
    let segments: Vec<_> = path.segments.iter().collect();
    matches!(segments.as_slice(), [.., ty, method]
        if ty.ident == "Metrics" && metric_methods().contains(&method.ident.to_string()))
}

fn is_self(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::Path(p) if p.path.is_ident("self"))
}

fn identifiers(tokens: &impl ToTokens) -> BTreeSet<String> {
    flat(tokens.to_token_stream())
        .into_iter()
        .filter_map(|t| match t {
            TokenTree::Ident(i) => Some(i.to_string()),
            _ => None,
        })
        .collect()
}

impl Finder<'_> {
    fn record(&mut self, kind: &str, value: &impl ToTokens) {
        self.entries.push(format!(
            "{} :: {} :: {kind} :: {}",
            self.path,
            self.scope.path(),
            token_text(value)
        ));
    }

    fn scoped(&mut self, visit: impl FnOnce(&mut Self)) {
        let depth = self.bindings.len();
        visit(self);
        self.bindings.truncate(depth);
    }

    fn tracks_bindings(&self) -> bool {
        self.path == "metrics.rs"
            || self.path.starts_with("metrics/")
            || (self.path == "http.rs" && self.scope.path() == "request_context")
    }

    /// A call of a `Metrics` method on a receiver other than the `METRICS`
    /// path: an alias, a `&Metrics` binding, a field or a sink. `self` is
    /// `Metrics` only inside its own `impl`; a call without arguments carries
    /// no label value.
    fn metric_receiver(&self, receiver_is_self: bool, method: &str, has_args: bool) -> bool {
        has_args
            && metric_methods().contains(method)
            && (!receiver_is_self || self.scope.path().starts_with("impl Metrics::"))
    }

    fn read_sources(&mut self, visit: impl FnOnce(&mut Self)) -> Reads {
        self.reads.push(Reads::new());
        visit(self);
        self.reads.pop().unwrap()
    }

    fn binding(&mut self, pat: &syn::Pat, value: Option<&syn::Expr>, reads: Reads) {
        if self.tracks_bindings() {
            #[derive(Default)]
            struct Names(BTreeSet<String>);
            impl Visit<'_> for Names {
                fn visit_pat_ident(&mut self, pat: &syn::PatIdent) {
                    self.0.insert(pat.ident.to_string());
                    visit::visit_pat_ident(self, pat);
                }
            }
            let mut names = Names::default();
            names.visit_pat(pat);
            let mut source = token_text(pat);
            if let Some(value) = value {
                source.push_str(&format!(" = {}", token_text(value)));
                // A shadowing initializer still reads the previous binding.
                for (name, previous) in reads {
                    if names.0.contains(&name) {
                        source.push_str(&format!(" [{previous}]"));
                    }
                }
            }
            self.bindings.push((self.scope.path(), names.0, source));
        }
    }

    fn visible(&self, name: &str) -> Option<&str> {
        let current = self.scope.path();
        self.bindings
            .iter()
            .rev()
            .find(|(scope, names, _)| *scope == current && names.contains(name))
            .map(|(_, _, source)| source.as_str())
    }

    // Direct METRICS calls also occur in select!, stream! and macro_rules!.
    fn macro_calls(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.macro_calls(group.stream());
            }
            if let [TokenTree::Ident(name), TokenTree::Group(args), ..] = &tokens[at..]
                && name == "emit"
                && args.delimiter() == Delimiter::Parenthesis
                && !matches!(&tokens[..at], [.., TokenTree::Punct(dot)] if dot.as_char() == '.')
                && !matches!(&tokens[..at], [.., TokenTree::Punct(a), TokenTree::Punct(b)] if a.as_char() == ':' && b.as_char() == ':')
            {
                self.record(
                    "macro event",
                    &tokens[at..at + 2].iter().cloned().collect::<TokenStream>(),
                );
            }
            if let [
                TokenTree::Ident(receiver),
                TokenTree::Punct(dot),
                TokenTree::Ident(method),
                TokenTree::Group(args),
                ..,
            ] = &tokens[at..]
                && dot.as_char() == '.'
                && args.delimiter() == Delimiter::Parenthesis
                && (receiver == "METRICS"
                    || method == "phase"
                    || self.metric_receiver(
                        receiver == "self",
                        &method.to_string(),
                        !args.stream().is_empty(),
                    ))
            {
                self.record(
                    if method == "phase" {
                        "macro phase label"
                    } else {
                        "macro metrics call"
                    },
                    &tokens[at..at + 4].iter().cloned().collect::<TokenStream>(),
                );
            }
            if let [
                TokenTree::Ident(ty),
                TokenTree::Punct(a),
                TokenTree::Punct(b),
                TokenTree::Ident(method),
                TokenTree::Group(args),
                ..,
            ] = &tokens[at..]
                && ty == "Metrics"
                && a.as_char() == ':'
                && b.as_char() == ':'
                && args.delimiter() == Delimiter::Parenthesis
                && metric_methods().contains(&method.to_string())
            {
                self.record(
                    "macro metrics call",
                    &tokens[at..at + 5].iter().cloned().collect::<TokenStream>(),
                );
            }
        }
    }

    fn label_use(&mut self, kind: &str, call: &syn::ExprMethodCall) {
        self.visit_expr(&call.receiver);
        self.label_args(kind, call, &call.args);
    }

    /// Record `node` with the visible sources of the bindings `args` read.
    fn label_args<'a>(
        &mut self,
        kind: &str,
        node: &impl ToTokens,
        args: impl IntoIterator<Item = &'a syn::Expr>,
    ) {
        self.record(kind, node);
        let entry = self.entries.len() - 1;
        let reads = self.read_sources(|this| {
            for arg in args {
                this.visit_expr(arg);
            }
        });
        for (name, source) in reads {
            self.entries[entry].push_str(&format!(" :: {name} from {source}"));
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    track_scope!();

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        // A call's own path is handled in `visit_expr_call`; here it is a
        // method passed as a value.
        if metrics_method_path(&path.path) {
            self.record("metrics method", path);
        }
        if !self.reads.is_empty()
            && let Some(name) = path.path.get_ident()
            && let Some(source) = self.visible(&name.to_string())
        {
            let read = (name.to_string(), source.to_owned());
            for reads in &mut self.reads {
                reads.insert(read.clone());
            }
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !self.tracks_bindings() {
            visit::visit_local(self, local);
            return;
        }
        if let Some(init) = &local.init {
            let reads = self.read_sources(|this| this.visit_expr(&init.expr));
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
            self.binding(&local.pat, Some(&init.expr), reads);
        } else {
            self.binding(&local.pat, None, Reads::new());
        }
    }

    fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        self.binding(&expr.pat, Some(&expr.expr), reads);
    }

    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.scoped(|this| {
            this.visit_expr(&expr.cond);
            this.visit_block(&expr.then_branch);
        });
        if let Some((_, branch)) = &expr.else_branch {
            self.visit_expr(branch);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        self.scoped(|this| visit::visit_expr_while(this, expr));
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast syn::ExprForLoop) {
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        self.scoped(|this| {
            this.binding(&expr.pat, Some(&expr.expr), reads);
            this.visit_block(&expr.body);
        });
    }

    fn visit_expr_closure(&mut self, expr: &'ast syn::ExprClosure) {
        self.scoped(|this| {
            for pat in &expr.inputs {
                this.binding(pat, None, Reads::new());
            }
            this.visit_expr(&expr.body);
        });
    }

    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        if !self.tracks_bindings() {
            visit::visit_expr_match(self, expr);
            return;
        }
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        for arm in &expr.arms {
            self.scoped(|this| {
                this.binding(&arm.pat, Some(&expr.expr), reads.clone());
                this.visit_arm(arm);
            });
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "get_or_create" || call.method == "get_or_create_owned" {
            self.label_use("family", call);
            return;
        } else if call.method == "collection_error"
            && self.scope.path().starts_with("impl Metrics::")
            && matches!(&*call.receiver, syn::Expr::Path(p) if p.path.is_ident("self"))
        {
            self.record("collection error label", call);
        } else if call.method == "phase" {
            self.record("phase label", call);
        } else if let syn::Expr::Path(p) = &*call.receiver
            && p.path.segments.last().is_some_and(|s| s.ident == "METRICS")
        {
            if call.method == "observe_http" {
                self.label_use("metrics call", call);
                return;
            }
            self.record("metrics call", call);
        } else if self.metric_receiver(
            is_self(&call.receiver),
            &call.method.to_string(),
            !call.args.is_empty(),
        ) {
            self.record("metrics call", call);
        } else if CONSTANT_LABELS.contains(&call.method.to_string().as_str()) {
            self.record("registry labels", call);
        } else if call.method == "attribute" && identifiers(&call.args).contains("Attribute") {
            self.record("span attribute", call);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func
            && let Some(name) = p.path.segments.last()
        {
            if name.ident == "emit"
                && (p.path.is_ident("emit") || p.path.segments.iter().any(|s| s.ident == "ops_log"))
            {
                self.record("event", call);
            }
            if self.path == "ops_log.rs" && name.ident == "log_record" {
                self.record("record inputs", call);
            }
            if self.path == "metrics/registry.rs" && name.ident == "series" {
                self.record("precreated labels", call);
            }
            // Call forms: `Family::get_or_create(&family, labels)` and
            // `Metrics::observe_cache(&METRICS, ..)`.
            if name.ident == "get_or_create" || name.ident == "get_or_create_owned" {
                self.label_args("family", call, &call.args);
                return;
            }
            if metrics_method_path(&p.path) {
                self.record("metrics call", call);
                for arg in &call.args {
                    self.visit_expr(arg);
                }
                return;
            }
            if CONSTANT_LABELS.contains(&name.ident.to_string().as_str()) {
                self.record("registry labels", call);
            }
            let segments: Vec<_> = p.path.segments.iter().collect();
            if matches!(segments.as_slice(), [.., ty, new] if ty.ident == "Info" && new.ident == "new")
            {
                self.record("info metric", call);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if self.path == "ops_log.rs" && expr.path.is_ident("Queued") {
            self.record("queued event", expr);
        }
        visit::visit_expr_struct(self, expr);
    }

    fn visit_expr_assign(&mut self, expr: &'ast syn::ExprAssign) {
        if self.path == "metrics_export.rs"
            && let syn::Expr::Index(index) = &*expr.left
            && let syn::Expr::Lit(key) = &*index.index
            && let syn::Lit::Str(key) = &key.lit
            && key.value() == "attributes"
        {
            self.record("OTLP labels", expr);
        }
        visit::visit_expr_assign(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // syn leaves macro bodies opaque. Pin the tokens of a macro containing
        // a producer or family access rather than silently omitting that site.
        self.macro_calls(mac.tokens.clone());
        let fixed_values = mac.path.is_ident("fixed_enum")
            && match self.path {
                // Telemetry's tables are span names and attribute values.
                "ops_log.rs" | "telemetry.rs" => true,
                "otlp.rs" | "metrics_export.rs" => {
                    let expected = if self.path == "otlp.rs" {
                        "Delivery"
                    } else {
                        "DropReason"
                    };
                    matches!(mac.tokens.clone().into_iter().next(), Some(TokenTree::Ident(name)) if name == expected)
                }
                _ => false,
            };
        let names = identifiers(&mac.tokens);
        let family = names.contains("get_or_create") || names.contains("get_or_create_owned");
        if fixed_values || family || names.contains("OpsEvent") || names.contains("ops_log") {
            self.record("macro", mac);
        }
        // Macro syntax is opaque. Alongside a family macro's complete tokens,
        // retain candidate inputs visible at its invocation. A macro in a
        // regular family argument can also read an enclosing binding.
        if family || !self.reads.is_empty() {
            for name in names {
                if let Some(source) = self.visible(&name).map(str::to_owned) {
                    if family {
                        self.entries
                            .last_mut()
                            .unwrap()
                            .push_str(&format!(" :: {name} from {source}"));
                    }
                    for reads in &mut self.reads {
                        reads.insert((name.clone(), source.clone()));
                    }
                }
            }
        }
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        // A collector encodes its own families and labels; a derived label
        // set is the label names of a family.
        let collector = matches!(item, syn::Item::Impl(i)
            if i.trait_.as_ref().is_some_and(|(t, _)| t.segments.last().is_some_and(|s| s.ident == "Collector")));
        let attrs = match item {
            syn::Item::Struct(i) => i.attrs.as_slice(),
            syn::Item::Enum(i) => i.attrs.as_slice(),
            _ => &[],
        };
        let label_set = attrs.iter().any(|a| {
            a.path().is_ident("derive")
                && identifiers(&a.meta)
                    .iter()
                    .any(|n| n == "EncodeLabelSet" || n == "EncodeLabelValue")
        });
        if collector {
            self.record("collector", item);
        } else if label_set {
            self.record("label set", item);
        }
        visit::visit_item(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if self.path == "ops_log.rs" && item.ident == "OpsEvent" {
            for variant in &item.variants {
                self.record("event variant", variant);
            }
        }
        if self.path == "telemetry.rs" && item.ident == "Attribute" {
            for variant in &item.variants {
                self.record("span attribute variant", variant);
            }
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        if self.path == "ops_log.rs"
            && self.scope.path() == "impl OpsEvent"
            && item.ident == "SLUGS"
        {
            self.record("event slugs", &item.expr);
        }
        visit::visit_impl_item_const(self, item);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let scope = self.scope.path();
        let wire = self.path == "ops_log.rs"
            && (matches!(
                scope.as_str(),
                "log_record"
                    | "impl Exporter::envelope"
                    | "impl ExportFailure::as_str"
                    | "impl Severity::text"
                    | "impl Severity::number"
            ) || scope.starts_with("impl OpsEvent::"));
        let label_helper = matches!(
            (self.path, scope.as_str()),
            (
                "metrics/requests.rs",
                "impl Metrics::labels" | "method_label"
            ) | (
                "metrics.rs",
                "bounded" | "denial_scope" | "admission_outcome"
            ) | ("otlp.rs", "resource_attributes")
                | (
                    "telemetry.rs",
                    "impl Attribute::value" | "impl Span::serialize" | "impl Exporter::body"
                )
                // The columns the attempt labels are read from.
                | ("governance/settlement.rs", "reservation")
        );
        if wire || label_helper {
            for (i, stmt) in block.stmts.iter().enumerate() {
                self.record(&format!("statement {}", i + 1), stmt);
            }
            return;
        }
        self.scoped(|this| visit::visit_block(this, block));
    }
}

/// The names of the methods of `Metrics` (its `impl` blocks in `metrics.rs`
/// and `metrics/`), read from the production tree: a call of one of these on
/// any receiver is a metric call.
fn metric_methods() -> &'static BTreeSet<String> {
    static NAMES: std::sync::LazyLock<BTreeSet<String>> = std::sync::LazyLock::new(|| {
        production("lib.rs")
            .iter()
            .filter(|s| s.path == "metrics.rs" || s.path.starts_with("metrics/"))
            .flat_map(|s| s.file.items.iter())
            .filter_map(|item| match item {
                syn::Item::Impl(i) if i.trait_.is_none() && is_metrics(&i.self_ty) => Some(i),
                _ => None,
            })
            .flat_map(|i| i.items.iter())
            .filter_map(|item| match item {
                syn::ImplItem::Fn(f) if f.sig.receiver().is_some() => Some(f.sig.ident.to_string()),
                _ => None,
            })
            .collect()
    });
    &NAMES
}

fn is_metrics(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Metrics"))
}

fn inventory(source: &Source) -> Vec<String> {
    let mut finder = Finder {
        path: &source.path,
        scope: Scope::default(),
        entries: Vec::new(),
        bindings: Vec::new(),
        reads: Vec::new(),
    };
    finder.visit_file(&source.file);
    finder.entries
}

#[test]
fn fields_match_reviewed_snapshot() {
    let mut entries: Vec<_> = ["lib.rs", "main.rs"]
        .into_iter()
        .flat_map(production)
        .flat_map(|source| inventory(&source))
        .collect();
    entries.sort();
    let snapshot = include_str!("telemetry_fields.snapshot");
    assert!(!entries.is_empty(), "no telemetry fields found");
    let changes = super::snapshot_diff(entries, snapshot);
    assert!(
        changes.is_empty(),
        "Review the changed telemetry fields and their sources before updating telemetry_fields.snapshot:\n{}",
        changes.join("\n")
    );
}

mod tests;
