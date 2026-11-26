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

/// The removed global mode's settlement prefix (catalog lock, installation
/// row) has no exception: both statements are findings again.
#[test]
fn the_former_global_job_prefix_is_refused() {
    let body = r#"if !scoped(store) {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx).await.map_err(record_storage)?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE",)
        .fetch_one(&mut *tx).await.map_err(record_storage)?;
}"#;
    let fixture = parse_fixture(
        "governance/jobs.rs",
        &format!("fn accept_job() {{ {body} }}"),
    );
    assert_eq!(violations(&fixture).len(), 2);
}

const AUTHORITY: &str = r#"const ADMISSION_SQL: &str = "SELECT 1";
    fn revalidate_admission() {
        sqlx::query_as(ADMISSION_SQL);
    }"#;

#[test]
fn authority_reader_scans_every_production_item() {
    let text = format!("{AUTHORITY} fn unrelated() {{}}");
    assert!(violations(&auth_query::source(&text)).is_empty());
    for unrelated in [
        r#"fn unrelated() { sqlx::query("SELECT 1 FROM users FOR SHARE"); }"#,
        r#"fn unrelated() { sqlx::query("SELECT lock_installation()"); }"#,
    ] {
        let text = format!("{AUTHORITY} {unrelated}");
        assert_eq!(
            violations(&auth_query::source(&text)).len(),
            1,
            "{unrelated}"
        );
    }
    // Test-only items are not production code.
    let text =
        format!(r#"{AUTHORITY} #[cfg(test)] fn probe() {{ sqlx::query("SELECT 1 FOR SHARE"); }}"#);
    assert!(violations(&auth_query::source(&text)).is_empty());
}

#[test]
#[should_panic(expected = "include!")]
fn authority_reader_rejects_unreadable_revalidation_code() {
    let text = AUTHORITY.replace("sqlx::query_as", r#"include!("extra.rs"); sqlx::query_as"#);
    auth_query::source(&text);
}

#[test]
fn the_admission_authority_query_takes_no_row_locks() {
    let source = AUTHORITY;
    assert!(violations(&parse_fixture("auth.rs", source)).is_empty());
    let documented = format!("/// Runs [`ADMISSION_SQL`].\n{source}");
    assert!(violations(&parse_fixture("auth.rs", &documented)).is_empty());
    for changed in [
        source.replace("SELECT 1", "SELECT 1 FOR SHARE OF k,w"),
        source.replace("SELECT 1", "SELECT 1 FoR KEY SHARE"),
        source.replace("SELECT 1", "SELECT 1 FOR UPDATE"),
        source.replace(r#""SELECT 1""#, r#"concat!("SELECT 1", " FOR SHARE")"#),
        source.replace(
            "ADMISSION_SQL);",
            r#"ADMISSION_SQL.to_owned() + " FOR SHARE");"#,
        ),
        source.replace("sqlx::query_as(ADMISSION_SQL)", "sqlx::query_as(OTHER_SQL)"),
        source.replace(
            "sqlx::query_as",
            "query(\"SELECT lock_installation()\"); sqlx::query_as",
        ),
        source.replace("fn revalidate_admission", "fn revalidate_admission_with"),
        // A second use of the query, wherever its lock clause comes from.
        source.replace(
            "sqlx::query_as(ADMISSION_SQL);",
            r#"sqlx::query_as(ADMISSION_SQL); sqlx::query_as(&format!("{}{}", ADMISSION_SQL, crate::db::SUFFIX));"#,
        ),
        source.replace(
            "sqlx::query_as(ADMISSION_SQL);",
            r#"let s = format!("{ADMISSION_SQL} FOR {}", "SHARE"); sqlx::query_as(&s); if false { sqlx::query_as(ADMISSION_SQL); }"#,
        ),
        source.replace(
            "sqlx::query_as(ADMISSION_SQL);",
            r#"sqlx::query_as(ADMISSION_SQL.to_owned() + " FOR" + " SHARE"); if false { sqlx::query_as(ADMISSION_SQL); }"#,
        ),
        format!("{source} fn other() {{ sqlx::query_as(ADMISSION_SQL); }}"),
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
    let (_, sql) = ROW_LOCKS
        .iter()
        .find(|(owner, _)| *owner == "governance::batch::close_batch_once")
        .unwrap();
    let owner = format!("fn close_batch_once() {{ query({sql:?}); }}");
    let read = |path: &std::path::Path| match path.to_str()? {
        "governance.rs" => Some(r#"#[path = "governance/batch.rs"] mod other;"#.to_owned()),
        "governance/batch.rs" => Some(owner.clone()),
        _ => None,
    };
    let files = tree::load_tree(&read, "governance.rs", false);
    assert_eq!(files.iter().flat_map(violations).count(), 1);
}
