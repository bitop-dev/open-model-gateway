//! #840: guardrail policy routes on real PostgreSQL: who writes and reads each
//! layer, the refusal reasons, tighten-only composition, `local_only` on write
//! and when composing, override reset, audit content and the cache topic.
use super::*;

const TYPE_TEAM: &str = "/api/v1/platform/workspace-types/team/guardrails";
fn platform_ws(ws: Uuid) -> String {
    format!("/api/v1/platform/workspaces/{ws}/guardrails")
}
fn local(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/guardrails")
}
fn secrets(action: &str) -> Value {
    json!({"name":"Secrets","protection":"secrets","detector":{"kind":"pattern","categories":["secret"]},"input":{"action":action}})
}
fn custom(pattern: &str) -> Value {
    json!({"name":"Code names","protection":"custom","detector":{"kind":"pattern","categories":["custom"],"patterns":[pattern]},"input":{"action":"block"}})
}
fn moderation() -> Value {
    json!({"name":"Moderation","protection":"moderation","detector":{"kind":"systemone","categories":["moderation"]},"input":{"action":"block"}})
}
fn reason(v: &Value) -> &str {
    v["error"]["reason"].as_str().unwrap_or("")
}
/// A PUT built from the layer as `u` reads it now: `expected` is filled in
/// (the layer's id and revision, or null) unless the body names one.
async fn put(s: &Store, u: &BrowserPrincipal, path: &str, mut body: Value) -> (StatusCode, Value) {
    if body.is_object() && body.get("expected").is_none() {
        body["expected"] = read_expected(s, u, path).await;
    }
    call(s, u, "PUT", path, body).await
}
/// A reset naming the override `u` reads now (none: no query).
async fn reset_override(s: &Store, u: &BrowserPrincipal, ws: Uuid) -> (StatusCode, Value) {
    let path = platform_ws(ws);
    let query = match read_expected(s, u, &path).await {
        Value::Null => String::new(),
        e => format!(
            "?rule_set_id={}&revision={}",
            e["rule_set_id"].as_str().unwrap(),
            e["revision"]
        ),
    };
    call(s, u, "DELETE", &format!("{path}{query}"), Value::Null).await
}
/// The `expected` of the layer at `path` as `u` reads it (null when there is
/// none, or `u` may not read it).
async fn read_expected(s: &Store, u: &BrowserPrincipal, path: &str) -> Value {
    let (status, v) = call(s, u, "GET", path, Value::Null).await;
    if !status.is_success() {
        return Value::Null;
    }
    let set = if path.contains("/workspace-types/") {
        &v["rule_set"]
    } else if path.contains("/platform/workspaces/") {
        &v["override"]
    } else {
        &v["local"]
    };
    if set.is_null() {
        Value::Null
    } else {
        json!({"rule_set_id": set["id"], "revision": set["revision"]})
    }
}
async fn version(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT version FROM config_versions WHERE topic='guardrails'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn platform_layers_are_written_by_admins_and_read_by_auditors(pool: PgPool) {
    let f = fixture(&pool).await;
    let before = version(&pool).await;
    let (status, body) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[secrets("block"), custom(r"PROJECT-\d{4}")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule_set"]["revision"], 1);
    assert_eq!(body["rule_set"]["local_only"], false);
    assert!(
        version(&pool).await > before,
        "the guardrails topic moves at commit"
    );
    let (status, read) = call(&f.s, &f.auditor, "GET", TYPE_TEAM, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        read["rule_set"]["rules"][1]["detector"]["patterns"],
        json!([r"PROJECT-\d{4}"]),
        "auditors read pattern text"
    );
    for (u, method) in [
        (&f.auditor, "PUT"),
        (&f.owner, "PUT"),
        (&f.owner, "GET"),
        (&f.member, "GET"),
    ] {
        let (status, _) = call(&f.s, u, method, TYPE_TEAM, json!({"rules":[]})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} by {}", u.email);
    }
    let (status, _) = put(
        &f.s,
        &f.admin,
        "/api/v1/platform/workspace-types/galaxy/guardrails",
        json!({"local_only":false,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A second write moves the revision forward and keeps ids.
    let id = read["rule_set"]["rules"][0]["id"].clone();
    let mut kept = secrets("monitor");
    kept["id"] = id.clone();
    let (status, body) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[kept]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rule_set"]["revision"], 2);
    assert_eq!(body["rule_set"]["rules"][0]["id"], id);
    let (status, detectors) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/guardrails/detectors",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pattern = detectors["detectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["kind"] == "pattern")
        .unwrap();
    assert_eq!(
        (pattern["available"].clone(), pattern["egress"].clone()),
        (json!(true), json!("local"))
    );
    let (status, _) = call(
        &f.s,
        &f.owner,
        "GET",
        "/api/v1/platform/guardrails/detectors",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[dbtest::test]
async fn the_local_layer_is_written_by_owners_and_admins_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[custom("internal-host-[0-9]+")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["local"]["rules"][0]["detector"]["patterns"],
        json!(["internal-host-[0-9]+"]),
        "the writer reads its patterns"
    );
    assert_eq!(body["enforced"], true, "input rules are enforced");
    let (status, _) = put(&f.s, &f.member, &local(f.team), json!({"rules":[]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, read) = call(&f.s, &f.member, "GET", &local(f.team), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["can_edit"], false);
    let rule = &read["effective"][0];
    assert_eq!(
        (
            rule["source"].clone(),
            rule["detector"]["pattern_count"].clone()
        ),
        (json!("workspace_local"), json!(1))
    );
    assert!(
        !read.to_string().contains("internal-host"),
        "members see a count, not the pattern"
    );
    // A non-member of a shared workspace is refused, as on its policy route.
    let (status, _) = call(&f.s, &f.outsider, "GET", &local(f.team), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (policy, _) = call(
        &f.s,
        &f.outsider,
        "GET",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(policy, status);
    // Platform staff read a shared workspace's policy but never write its local layer.
    let (status, staff) = call(&f.s, &f.auditor, "GET", &local(f.team), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(staff["can_edit"], false);
    let (status, _) = put(&f.s, &f.admin, &local(f.team), json!({"rules":[]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[dbtest::test]
async fn a_personal_owner_tightens_privately(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.personal),
        json!({"rules":[custom("my-secret-code")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Platform staff see the platform layers of a personal workspace only.
    for u in [&f.admin, &f.auditor] {
        let (status, read) = call(&f.s, u, "GET", &platform_ws(f.personal), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (
                read["local"].clone(),
                read["local_visible"].clone(),
                read["effective"].clone()
            ),
            (Value::Null, json!(false), Value::Null)
        );
        assert!(!read.to_string().contains("my-secret-code"));
        let (status, _) = call(&f.s, u, "GET", &local(f.personal), Value::Null).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the personal workspace route is owner-only"
        );
    }
}

#[dbtest::test]
async fn a_local_layer_cannot_loosen_a_platform_rule(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, platform) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[secrets("monitor")]}),
    )
    .await;
    let target = platform["rule_set"]["rules"][0]["id"].clone();
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":target,"input":{"action":"off"}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(reason(&body), "guardrail_loosen_not_allowed");
    assert_eq!(body["error"]["rule"], target);
    let mut replaced = secrets("off");
    replaced["id"] = target.clone();
    let (status, body) = put(&f.s, &f.owner, &local(f.team), json!({"rules":[replaced]})).await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::FORBIDDEN, "guardrail_loosen_not_allowed")
    );
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":target,"input":{"action":"block"}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let effective = &body["effective"][0];
    assert_eq!(effective["id"], target);
    assert_eq!(
        effective["input"]["action"], "block",
        "the tightened rule runs stricter"
    );
    assert_eq!(
        effective["tightened_by"],
        json!([body["local"]["rules"][0]["id"]])
    );
    // The platform rule is removed: the entry stays, inactive.
    put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[]}),
    )
    .await;
    let (_, read) = call(&f.s, &f.owner, "GET", &local(f.team), Value::Null).await;
    assert_eq!(
        read["effective"][0]["inactive_reason"],
        "tightened_rule_missing"
    );
    assert_eq!(read["effective"][0]["active"], false);
}

#[dbtest::test]
async fn later_phase_rules_and_egress_are_refused_on_write(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[moderation()]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::BAD_REQUEST, "guardrail_detector_unavailable"),
        "{body}"
    );
    let mut redact = secrets("redact");
    let (status, body) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[redact.clone()]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::BAD_REQUEST, "guardrail_detector_unavailable")
    );
    redact["input"]["action"] = json!("block");
    redact["output"] = json!({"mode":"monitor"});
    let (status, body) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[redact]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::BAD_REQUEST, "guardrail_detector_unavailable")
    );
    let (status, body) = put(
        &f.s,
        &f.admin,
        &platform_ws(f.team),
        json!({"local_only":true,"rules":[moderation()]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::FORBIDDEN, "guardrail_egress_not_allowed")
    );
    let (status, _) = put(
        &f.s,
        &f.admin,
        &platform_ws(f.team),
        json!({"local_only":true,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[moderation()]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::FORBIDDEN, "guardrail_egress_not_allowed"),
        "{body}"
    );
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[custom("(unclosed")]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::BAD_REQUEST, "guardrail_pattern_invalid")
    );
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"local_only":true,"rules":[]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::BAD_REQUEST, ""),
        "local_only is a platform field"
    );
    // Nothing was stored by any refusal.
    let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_rules")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rules, 0);
}

#[dbtest::test]
async fn local_only_makes_an_earlier_egress_rule_inactive_when_composing(pool: PgPool) {
    let f = fixture(&pool).await;
    put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[secrets("block")]}),
    )
    .await;
    // A rule an earlier writer stored with a detector that sends text out (a
    // later phase, or a later release): written directly, as phase 1 refuses it.
    let set: Uuid = sqlx::query_scalar(
        "SELECT id FROM guardrail_rule_sets WHERE layer='workspace_local' AND workspace_id=$1",
    )
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    let egress = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,1,'Moderation','moderation','systemone','{moderation}','block','user_text','off','fail_closed','{generation}','deny')")
        .bind(egress).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let (_, open) = call(&f.s, &f.owner, "GET", &local(f.team), Value::Null).await;
    // Allowed without `local_only`, but this release cannot run it: never
    // shown active, with the reason named.
    let later = open["effective"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(egress))
        .unwrap();
    assert_eq!(
        (later["active"].clone(), later["inactive_reason"].clone()),
        (json!(false), json!("not_runnable"))
    );
    let (status, _) = put(
        &f.s,
        &f.admin,
        &platform_ws(f.team),
        json!({"local_only":true,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, closed) = call(&f.s, &f.owner, "GET", &local(f.team), Value::Null).await;
    assert_eq!(closed["egress"]["local_only"], true);
    let rows = closed["effective"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "shown, never dropped");
    let inactive = rows.iter().find(|r| r["id"] == json!(egress)).unwrap();
    assert_eq!(
        (
            inactive["active"].clone(),
            inactive["inactive_reason"].clone()
        ),
        (json!(false), json!("egress_not_allowed"))
    );
    assert!(
        rows.iter()
            .any(|r| r["active"] == true && r["detector"]["kind"] == "pattern")
    );
}

#[dbtest::test]
async fn an_override_replaces_the_type_default_until_reset(pool: PgPool) {
    let f = fixture(&pool).await;
    put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[secrets("monitor")]}),
    )
    .await;
    let (status, body) = put(
        &f.s,
        &f.admin,
        &platform_ws(f.team),
        json!({"local_only":false,"rules":[secrets("block"), custom("acme-[a-z]+")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["platform"]["source"], "workspace_override");
    assert_eq!(body["effective"].as_array().unwrap().len(), 2);
    assert_eq!(body["effective"][0]["input"]["action"], "block");
    let (status, body) = reset_override(&f.s, &f.admin, f.team).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["platform"]["source"], "type_default");
    assert_eq!(body["override"], Value::Null);
    assert_eq!(body["effective"][0]["input"]["action"], "monitor");
    // Audited with ids, actions and a pattern count; never a name or a pattern.
    let events: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='guardrail_policy.updated' ORDER BY created_at")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1]["layer"], "workspace_override");
    assert_eq!(events[1]["custom_patterns"], 1);
    assert_eq!(events[1]["rules"][0]["action"], "block");
    assert_eq!(events[2]["reset"], true);
    let all = serde_json::to_string(&events).unwrap();
    assert!(!all.contains("acme") && !all.contains("Secrets"), "{all}");
    let (status, _) = call(
        &f.s,
        &f.auditor,
        "DELETE",
        &platform_ws(f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[dbtest::test]
async fn the_database_refuses_writes_the_routes_would_refuse(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = Uuid::now_v7();
    // A local layer written by someone who is not an owner or admin there.
    let err = sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,updated_by) VALUES($1,'workspace_local',$2,$3)")
        .bind(id).bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("owner or admin"), "{err}");
    let err = sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default','team',false,$2)")
        .bind(id).bind(f.owner.user_id).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("Platform Admin"), "{err}");
    let err = sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,local_only,updated_by) VALUES($1,'workspace_local',$2,true,$3)")
        .bind(id).bind(f.team).bind(f.owner.user_id).execute(&pool).await.unwrap_err();
    assert!(
        err.to_string().contains("check"),
        "local_only is a platform field: {err}"
    );
    // A tightening entry names a rule of its own workspace's platform layer.
    put(
        &f.s,
        &f.admin,
        &platform_ws(f.project),
        json!({"local_only":false,"rules":[secrets("monitor")]}),
    )
    .await;
    let other: Uuid = sqlx::query_scalar("SELECT r.id FROM guardrail_rules r JOIN guardrail_rule_sets s ON s.id=r.rule_set_id WHERE s.workspace_id=$1").bind(f.project).fetch_one(&pool).await.unwrap();
    put(&f.s, &f.owner, &local(f.team), json!({"rules":[]})).await;
    let set: Uuid = sqlx::query_scalar(
        "SELECT id FROM guardrail_rule_sets WHERE layer='workspace_local' AND workspace_id=$1",
    )
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,input_action) VALUES($1,$2,0,$3,'block')")
        .bind(Uuid::now_v7()).bind(set).bind(other).execute(&mut *tx).await.unwrap_err();
    assert!(err.to_string().contains("platform layer"), "{err}");
    drop(tx);
    // Only an override is removed; rules are never edited in place.
    let err = sqlx::query("DELETE FROM guardrail_rule_sets WHERE id=$1")
        .bind(set)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("only a guardrail override"),
        "{err}"
    );
}

const FULL_RULE: &str = "INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,$3,'Custom','custom','pattern','{custom}','block','user_text','off','fail_closed','{generation}','deny',$4)";

#[dbtest::test]
async fn rules_change_only_with_their_sets_revision(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, _) = put(
        &f.s,
        &f.admin,
        &platform_ws(f.project),
        json!({"local_only":false,"rules":[secrets("monitor")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let set: Uuid = sqlx::query_scalar(
        "SELECT id FROM guardrail_rule_sets WHERE layer='workspace_override' AND workspace_id=$1",
    )
    .bind(f.project)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Outside the transaction that moved the set's revision, its rules are
    // neither added nor removed, and the override is not removed.
    let err = sqlx::query(FULL_RULE)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind(1_i16)
        .bind(vec!["x".to_owned()])
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rule set's revision"), "{err}");
    let err = sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
        .bind(set)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rule set's revision"), "{err}");
    let err = sqlx::query("DELETE FROM guardrail_rule_sets WHERE id=$1")
        .bind(set)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("moved its revision"), "{err}");
    // Inside it, the database still holds the per-set limits and shapes.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.admin.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    for (categories, patterns) in [
        ("{custom}", "{}"),
        ("{secret}", "{x}"),
        ("{custom}", "{{x}}"),
    ] {
        let mut sp = sqlx::Acquire::begin(&mut *tx).await.unwrap();
        let err = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,3,'R','custom','pattern',$3::text[],'block','user_text','off','fail_closed','{generation}','deny',$4::text[])")
            .bind(Uuid::now_v7()).bind(set).bind(categories).bind(patterns)
            .execute(&mut *sp).await.unwrap_err();
        assert!(
            err.to_string().contains("check constraint"),
            "{categories} {patterns}: {err}"
        );
        sp.rollback().await.unwrap();
    }
    sqlx::query(FULL_RULE)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind(1_i16)
        .bind((0..50).map(|i| format!("p{i}")).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut sp = sqlx::Acquire::begin(&mut *tx).await.unwrap();
    let err = sqlx::query(FULL_RULE)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind(2_i16)
        .bind(vec!["one too many".to_owned()])
        .execute(&mut *sp)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at most 50"), "{err}");
    sp.rollback().await.unwrap();
    // Two entries so far; 48 more fill the set, and one beyond is refused.
    const SECRET_RULE: &str = "INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,$3,'Secrets','secrets','pattern','{secret}','monitor','user_text','off','fail_open','{generation}','deny')";
    for position in 2..50_i16 {
        sqlx::query(SECRET_RULE)
            .bind(Uuid::now_v7())
            .bind(set)
            .bind(position)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let err = sqlx::query(SECRET_RULE)
        .bind(Uuid::now_v7())
        .bind(set)
        .bind(50_i16)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at most 50"), "{err}");
    drop(tx);
    // The route removes an override in one transaction with its revision.
    let (status, reset) = reset_override(&f.s, &f.admin, f.project).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(reset["platform"]["source"], "type_default");
    // A pattern PostgreSQL cannot store is refused as a pattern, not a fault.
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[custom("a\u{0}b")]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["reason"], "guardrail_pattern_invalid");
}

/// A layer write names the layer it was built from, by id and revision, and
/// is refused (409) when the layer stored now is another one: a page or
/// script that read an older layer never replaces a newer one. A missing
/// `expected` is refused (400) only after authorization.
#[dbtest::test]
async fn a_write_built_from_an_older_layer_is_refused(pool: PgPool) {
    let f = fixture(&pool).await;
    let at = |id: &Value, revision: i64| json!({"rule_set_id": id, "revision": revision});
    let body = |expected: Value| json!({"local_only":false,"expected":expected,"rules":[secrets("monitor")]});
    let write = |path: String, expected: Value| {
        let s = f.s.clone();
        let u = f.admin.clone();
        async move { call(&s, &u, "PUT", &path, body(expected)).await }
    };

    // No `expected` at all: refused, but only once the caller may write.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PUT",
        TYPE_TEAM,
        json!({"local_only":false,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(reason(&v), "guardrail_expected_required");
    let (status, _) = call(
        &f.s,
        &f.auditor,
        "PUT",
        TYPE_TEAM,
        json!({"local_only":false,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The type default: created from no layer, then only from its current revision.
    let (status, v) = write(TYPE_TEAM.into(), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let id = v["rule_set"]["id"].clone();
    assert_eq!(v["rule_set"]["revision"], 1);
    // A revision is at least 1: zero is a malformed body, never a mismatch.
    let (status, v) = write(TYPE_TEAM.into(), at(&id, 0)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    for stale in [Value::Null, at(&json!(Uuid::new_v4()), 1)] {
        let (status, v) = write(TYPE_TEAM.into(), stale.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{stale}: {v}");
        assert_eq!(reason(&v), "guardrail_layer_changed");
    }
    let (status, v) = write(TYPE_TEAM.into(), at(&id, 1)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["rule_set"]["revision"], 2);
    let (status, _) = write(TYPE_TEAM.into(), at(&id, 1)).await;
    assert_eq!(status, StatusCode::CONFLICT, "a revision already replaced");

    // An override removed and created again starts over at revision 1 under a
    // new id: the old id with the same revision is refused.
    let path = platform_ws(f.team);
    let (status, v) = write(path.clone(), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let first = v["override"]["id"].clone();
    let reset = |q: String| {
        let s = f.s.clone();
        let u = f.admin.clone();
        let path = format!("{}{q}", platform_ws(f.team));
        async move { call(&s, &u, "DELETE", &path, Value::Null).await }
    };
    let (status, v) = reset(String::new()).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a reset names the override it removes: {v}"
    );
    assert_eq!(reason(&v), "guardrail_layer_changed");
    let (status, v) = reset(format!("?rule_set_id={}", first.as_str().unwrap())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "id without revision: {v}");
    let (status, _) = call(
        &f.s,
        &f.admin,
        "DELETE",
        &format!("{}?revision=0", platform_ws(Uuid::new_v4())),
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the workspace is checked first"
    );
    let (status, v) = reset(format!(
        "?rule_set_id={}&revision=1",
        first.as_str().unwrap()
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v["override"].is_null());
    let (status, _) = reset(format!(
        "?rule_set_id={}&revision=1",
        first.as_str().unwrap()
    ))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "no override left to remove");
    let (status, v) = write(path.clone(), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let second = v["override"]["id"].clone();
    assert_ne!(first, second);
    assert_eq!(v["override"]["revision"], 1);
    let (status, _) = write(path.clone(), at(&first, 1)).await;
    assert_eq!(status, StatusCode::CONFLICT, "the removed override's id");
    let (status, v) = write(path.clone(), at(&second, 1)).await;
    assert_eq!(status, StatusCode::OK, "{v}");

    // The local layer, written by a workspace admin.
    let local_body =
        |expected: Value| json!({"expected":expected,"rules":[custom(r"TICKET-\d{4}")]});
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &local(f.team),
        local_body(Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let lid = v["local"]["id"].clone();
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &local(f.team),
        local_body(Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &local(f.team),
        local_body(at(&lid, 1)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["local"]["revision"], 2);
}

/// Two writers built from the same revision: one write lands, the other is
/// refused, whatever the order the database serves them in.
#[dbtest::test]
async fn of_two_writes_built_from_one_revision_one_lands(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PUT",
        TYPE_TEAM,
        json!({"local_only":false,"expected":null,"rules":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let id = v["rule_set"]["id"].clone();
    for revision in 1..=5_i64 {
        let expected = json!({"rule_set_id": id, "revision": revision});
        let a = call(
            &f.s,
            &f.admin,
            "PUT",
            TYPE_TEAM,
            json!({"local_only":false,"expected":expected,"rules":[secrets("monitor")]}),
        );
        let b = call(
            &f.s,
            &f.admin,
            "PUT",
            TYPE_TEAM,
            json!({"local_only":false,"expected":expected,"rules":[secrets("block")]}),
        );
        let ((sa, va), (sb, vb)) = tokio::join!(a, b);
        let mut statuses = [sa, sb];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::OK, StatusCode::CONFLICT],
            "{va} {vb}"
        );
        let current: i64 = sqlx::query_scalar(
            "SELECT revision::bigint FROM guardrail_rule_sets WHERE layer='type_default' AND kind='team'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            current,
            revision + 1,
            "exactly one write moved the revision"
        );
    }
}

/// #841: a personal-data rule is written with the kinds it looks for and
/// read back with them (labels, for every reader); one that names none, or
/// names kinds on another category, is refused.
#[dbtest::test]
async fn a_personal_data_rule_is_written_and_read_with_its_kinds(pool: PgPool) {
    let f = fixture(&pool).await;
    let personal = |kinds: Value| json!({"name":"Personal data","protection":"personal_data","detector":{"kind":"pattern","categories":["personal_data"],"kinds":kinds},"input":{"action":"block"}});
    let (status, v) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[personal(json!(["card","us_ssn"]))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        v["rule_set"]["rules"][0]["detector"]["kinds"],
        json!(["card", "us_ssn"])
    );
    let (status, read) = call(&f.s, &f.member, "GET", &local(f.team), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        read["effective"][0]["detector"]["kinds"],
        json!(["card", "us_ssn"])
    );
    for bad in [
        personal(json!([])),
        personal(json!(["card", "card"])),
        personal(json!(["passport"])),
        json!({"name":"Secrets","protection":"secrets","detector":{"kind":"pattern","categories":["secret"],"kinds":["email"]},"input":{"action":"block"}}),
    ] {
        let (status, v) = put(
            &f.s,
            &f.admin,
            TYPE_TEAM,
            json!({"local_only":false,"rules":[bad.clone()]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    // Redaction is not available in this release, for personal data too.
    let mut redact = personal(json!(["email"]));
    redact["input"]["action"] = json!("redact");
    let (status, v) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[redact]}),
    )
    .await;
    assert_eq!(
        (status, reason(&v)),
        (StatusCode::BAD_REQUEST, "guardrail_detector_unavailable"),
        "{v}"
    );
    // A tightening names only what it tightens: never kinds, which would
    // change what the platform rule looks for.
    let (status, platform) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[personal(json!(["card"]))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{platform}");
    let target = platform["rule_set"]["rules"][0]["id"].clone();
    assert!(target.is_string(), "{platform}");
    for tightening in [
        json!({"tightens":target,"kinds":["email"]}),
        json!({"tightens":target,"detector":{"kind":"pattern","categories":["personal_data"],"kinds":["email"]}}),
    ] {
        let (status, v) = put(
            &f.s,
            &f.owner,
            &local(f.team),
            json!({"rules":[tightening.clone()]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tightening}: {v}");
    }
    // The same tightening without kinds is accepted.
    let (status, v) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":target,"input":{"action":"block"}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // A rule may look for personal data and secrets at once (through the
    // API): it keeps both, with its kinds.
    let both = json!({"name":"Both","protection":"personal_data","detector":{"kind":"pattern","categories":["personal_data","secret"],"kinds":["email"]},"input":{"action":"block"}});
    let (status, v) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[both]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let detector = &v["rule_set"]["rules"][0]["detector"];
    assert_eq!(
        detector["categories"],
        json!(["secret", "personal_data"]),
        "{v}"
    );
    assert_eq!(detector["kinds"], json!(["email"]), "{v}");
}

/// #841: `allow` is the weaker value of `uninspectable`. A local layer may
/// move a platform rule from `allow` to `deny` and the rule runs with
/// `deny`; `allow` on a tightening is refused as a loosening.
#[dbtest::test]
async fn a_local_layer_may_refuse_what_a_platform_rule_lets_through_unread(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut lets = secrets("block");
    lets["uninspectable"] = json!("allow");
    lets["workloads"] = json!(["generation", "images"]);
    let (status, platform) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[lets]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{platform}");
    let target = platform["rule_set"]["rules"][0]["id"].clone();
    assert_eq!(platform["rule_set"]["rules"][0]["uninspectable"], "allow");
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":target,"uninspectable":"allow"}]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::FORBIDDEN, "guardrail_loosen_not_allowed")
    );
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":target,"uninspectable":"deny"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["local"]["rules"][0]["uninspectable"], "deny");
    assert_eq!(body["effective"][0]["uninspectable"], "deny", "{body}");
    // A platform rule that already refuses it: deny again is accepted and
    // changes nothing; allow is refused.
    let (_, platform) = put(
        &f.s,
        &f.admin,
        TYPE_TEAM,
        json!({"local_only":false,"rules":[secrets("block")]}),
    )
    .await;
    let strict = platform["rule_set"]["rules"][0]["id"].clone();
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":strict,"uninspectable":"allow"}]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::FORBIDDEN, "guardrail_loosen_not_allowed")
    );
    let (status, body) = put(
        &f.s,
        &f.owner,
        &local(f.team),
        json!({"rules":[{"tightens":strict,"uninspectable":"deny"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["effective"][0]["uninspectable"], "deny");
    // The database refuses a direct write of allow on a tightening.
    let set: Uuid = sqlx::query_scalar(
        "SELECT id FROM guardrail_rule_sets WHERE layer='workspace_local' AND workspace_id=$1",
    )
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,uninspectable) VALUES($1,$2,10,$3,'allow')")
        .bind(Uuid::now_v7())
        .bind(set)
        .bind(Uuid::parse_str(strict.as_str().unwrap()).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(
        err.as_database_error().and_then(|e| e.constraint()),
        Some("guardrail_rules_tightening")
    );
}
