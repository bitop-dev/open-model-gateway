//! The authority reads of admission and candidate planning take no row
//! locks: every production item of `auth.rs` is scanned like governance code
//! (no lock clause, advisory lock or installation lock in any statement), and
//! `ADMISSION_SQL`, the one-round-trip admission query, must stay a literal
//! read by `revalidate_admission`.
use super::*;

pub(super) fn source(text: &str) -> Source {
    let path = "auth.rs";
    let file = tree::production_items(std::path::Path::new(path), text, |_| true)
        .expect("auth.rs must be present in production");
    Source {
        path: path.into(),
        module: "auth".into(),
        file,
        bin: false,
    }
}

pub(super) fn scan(scan: &mut Scan<'_>, file: &syn::File) {
    let mut constants = 0;
    let mut functions = 0;
    for item in &file.items {
        match item {
            syn::Item::Const(item) if item.ident.unraw() == "ADMISSION_SQL" => {
                constants += 1;
                if !matches!(&*item.expr, Expr::Lit(expr) if matches!(expr.lit, syn::Lit::Str(_))) {
                    scan.findings
                        .push("auth.rs: expected literal ADMISSION_SQL".into());
                }
            }
            syn::Item::Fn(f) if f.sig.ident.unraw() == "revalidate_admission" => {
                functions += 1;
                let expected = quote!(sqlx::query_as(ADMISSION_SQL)).to_string();
                let tokens = f.block.to_token_stream().to_string();
                if tokens.matches(&expected).count() != 1 {
                    scan.findings.push(
                        "auth.rs: revalidate_admission must run ADMISSION_SQL as written".into(),
                    );
                }
            }
            _ => {}
        }
    }
    if (constants, functions) != (1, 1) {
        scan.findings
            .push("auth.rs: expected one admission query and revalidation function".into());
    }
    scan.visit_file(file);
}
