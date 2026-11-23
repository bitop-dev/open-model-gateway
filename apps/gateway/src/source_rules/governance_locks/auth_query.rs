//! Check the scoped form of the authority query and the two expressions
//! that construct and select it. Other auth functions serve different paths.
use super::*;

const KEY_SHARE: &str = " FOR SHARE OF k,w";
const SHARE: &str = " FOR SHARE";

pub(super) fn scan(scan: &mut Scan<'_>, file: &syn::File) {
    let mut constants = 0;
    let mut functions = 0;
    for item in &file.items {
        match item {
            syn::Item::Const(item) if item.ident.unraw() == "ADMISSION_SQL" => {
                constants += 1;
                if let Expr::Lit(expr) = &*item.expr
                    && let syn::Lit::Str(sql) = &expr.lit
                {
                    let scoped = sql.value().replace(KEY_SHARE, "").replace(SHARE, "");
                    if let Some(rule) = sql_rule(&scoped, false) {
                        scan.findings
                            .push(format!("auth.rs ADMISSION_SQL in scoped mode: {rule}"));
                    }
                } else {
                    scan.findings
                        .push("auth.rs: expected literal ADMISSION_SQL".into());
                }
            }
            syn::Item::Fn(f) if f.sig.ident.unraw() == "revalidate_admission_with" => {
                functions += 1;
                let expected: syn::Stmt = syn::parse_quote!(
                    static UNLOCKED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
                        ADMISSION_SQL.replace(#KEY_SHARE, "").replace(#SHARE, "")
                    });
                );
                let expected = expected.to_token_stream().to_string();
                let mut initializers = 0;
                for stmt in &f.block.stmts {
                    if matches!(stmt, syn::Stmt::Item(syn::Item::Static(_)))
                        && stmt.to_token_stream().to_string() == expected
                    {
                        initializers += 1;
                    } else {
                        scan.visit_stmt(stmt);
                    }
                }
                if initializers != 1 || !scan.used.contains(SELECTION) {
                    scan.findings.push(
                        "auth.rs: inspect authority query construction and mode selection".into(),
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
}

pub(super) const SELECTION: &str = "auth::revalidate_admission_with::selection";

pub(super) fn selection(call: &syn::ExprCall) -> bool {
    call.to_token_stream().to_string()
        == quote!(sqlx::query_as(if lock {
            ADMISSION_SQL
        } else {
            UNLOCKED.as_str()
        }))
        .to_string()
}
