use super::*;

#[test]
fn unlisted_row_table_and_installation_locks() {
    for clause in [
        "FOR UPDATE",
        "for /*a*/ no KEY --b\n update",
        "FOR KEY SHARE",
        "LOCK /*x*/ TABLE installations",
        r#"SELECT public."lock_installation" ()"#,
    ] {
        let rust = format!("fn settle() {{ query({clause:?}); }}");
        assert_eq!(
            violations(&parse_fixture("governance/settlement.rs", &rust)).len(),
            1,
            "{clause}"
        );
    }
}

const GLOBAL: &str = r#"if !scoped(store) {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx).await.map_err(record_storage)?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE",)
        .fetch_one(&mut *tx).await.map_err(record_storage)?;
}"#;

#[test]
fn job_installation_and_catalog_exceptions_are_consumed_together() {
    for (body, count) in [
        (GLOBAL.to_owned(), 0),
        (GLOBAL.replace("}", "query(\"FOR UPDATE\"); }"), 1),
        (format!("{GLOBAL} {GLOBAL}"), 2),
        (GLOBAL.replace("!scoped", "scoped"), 2),
        (
            format!("{GLOBAL} else {{ query(\"SELECT lock_installation()\"); }}"),
            1,
        ),
    ] {
        let fixture = parse_fixture(
            "governance/jobs.rs",
            &format!("fn accept_job() {{ {body} }}"),
        );
        assert_eq!(violations(&fixture).len(), count, "{body}");
    }
}

const AUTHORITY: &str = r#"const ADMISSION_SQL: &str = "SELECT 1 FOR SHARE OF k,w";
    fn revalidate_admission_with() {
        static UNLOCKED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            ADMISSION_SQL.replace(" FOR SHARE OF k,w", "").replace(" FOR SHARE", "")
        });
        sqlx::query_as(if lock { ADMISSION_SQL } else { UNLOCKED.as_str() });
    }"#;

#[test]
fn authority_reader_limits_strict_checks_to_inventoried_items() {
    for unrelated in [
        "mod unrelated;",
        r#"fn unrelated() { include!("generated.rs"); }"#,
        "fn unrelated() { mod local; }",
    ] {
        let text = format!("{AUTHORITY} {unrelated}");
        assert!(violations(&auth_query::source(&text)).is_empty());
    }
}

#[test]
#[should_panic(expected = "include!")]
fn authority_reader_rejects_unreadable_revalidation_code() {
    let text = AUTHORITY.replace("sqlx::query_as", r#"include!("extra.rs"); sqlx::query_as"#);
    auth_query::source(&text);
}

#[test]
fn scoped_authority_query_and_mode_selection_are_checked() {
    let source = AUTHORITY;
    assert!(violations(&parse_fixture("auth.rs", source)).is_empty());
    for changed in [
        source.replace("SELECT 1 FOR SHARE", "SELECT 1 FOR UPDATE"),
        source.replace("SELECT 1 FOR SHARE", "SELECT 1 FoR SHARE"),
        source.replace("if lock", "if !lock"),
        source.replace("UNLOCKED.as_str()", "ADMISSION_SQL"),
        source.replace(".replace(\" FOR SHARE\", \"\")", ""),
        source.replace(
            "sqlx::query_as",
            "query(\"SELECT lock_installation()\"); sqlx::query_as",
        ),
    ] {
        assert!(
            !violations(&parse_fixture("auth.rs", &changed)).is_empty(),
            "{changed}"
        );
    }
}

#[test]
fn inventoried_row_locks_are_bound_to_the_query_and_function() {
    for (qualified, sql) in ROW_LOCKS {
        let (module, owner) = qualified.rsplit_once("::").unwrap();
        let path = format!("{}.rs", module.replace("::", "/"));
        let statement = format!("query({sql:?});");
        for (name, body, expected) in [
            (owner, statement.clone(), 0),
            (owner, statement.repeat(2), 1),
            ("other", statement.clone(), 1),
            (owner, statement.replace(" FOR ", " AND true FOR "), 1),
        ] {
            let rust = format!("fn {name}() {{ {body} }}");
            assert_eq!(
                violations(&parse_fixture(&path, &rust)).len(),
                expected,
                "{qualified}: {rust}"
            );
        }
    }
}

#[test]
fn path_attribute_does_not_grant_module_identity() {
    let read = |path: &std::path::Path| match path.to_str()? {
        "governance.rs" => Some(r#"#[path = "governance/jobs.rs"] mod other;"#.to_owned()),
        "governance/jobs.rs" => Some(format!("fn accept_job() {{ {GLOBAL} }}")),
        _ => None,
    };
    let files = tree::load_tree(&read, "governance.rs", false);
    assert_eq!(files.iter().flat_map(violations).count(), 2);
}
