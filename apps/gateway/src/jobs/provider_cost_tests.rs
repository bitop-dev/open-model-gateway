//! #52 part 2: a video whose provider reports its cost and no duration
//! (OpenRouter) is charged that figure, under the settlement rules: the
//! exact figure, marked as the provider's; unknown when absent or
//! unreadable; unknown when zero on a paid route, zero on a free one; and
//! above the hold, settled and marked unbounded.
use super::db_tests::{Fake, jobs, line, na, upstream_video, video_request, video_setup};
use super::*;
use crate::{
    billing::v3::Meter,
    governance::tests::db::{Fixture, amounts, fixture},
};
use serde_json::{Value, json};
use sqlx::PgPool;

/// A video completed with `cost`, polled once; its reservation as
/// `amounts` reads it and its settlement ledger entry's evidence.
async fn settled_with(
    f: &Fixture,
    fake: &Fake,
    j: &Jobs,
    cost: ReportedCost,
) -> ((String, Option<i64>, Option<i64>, bool), Option<String>) {
    fake.push_video(upstream_video(JobState::Queued, None));
    let request_id = Uuid::new_v4();
    j.create_video(f.principal, video_request(), request_id)
        .await
        .unwrap();
    let mut done = upstream_video(JobState::Completed, None);
    done.cost = Some(cost);
    fake.push_video(done);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    let evidence: Option<String> = sqlx::query_scalar(
        "SELECT evidence FROM monetary_ledger WHERE execution_id=$1 AND kind IN('settlement','unknown')",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    (amounts(f, request_id).await, evidence)
}

#[dbtest::test]
async fn a_reported_cost_is_the_charge_under_the_settlement_rules(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('personal',20) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs")
        .execute(&f.store.pool).await.unwrap();
    let fake = Fake::default();
    let j = jobs(&f, &fake, true).await;
    // The hold: 8 s at the highest size rate ($0.50/s).
    let held = Some(4_000_000);
    // The provider's figure, exact, marked as the provider's.
    let (state, evidence) = settled_with(&f, &fake, &j, ReportedCost::Known(288_001)).await;
    assert_eq!(state, ("settled".into(), held, Some(288_001), false));
    assert_eq!(evidence.as_deref(), Some("provider_reported_cost"));
    let components: Value = sqlx::query_scalar("SELECT r.cost_components FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id ORDER BY j.created_at DESC LIMIT 1")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        components["output_video_microusd"], "288001",
        "{components}"
    );
    // The length is not reported: recorded unknown, never the requested one.
    let observed: Value = sqlx::query_scalar("SELECT r.meter_usage FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id ORDER BY j.created_at DESC LIMIT 1")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert!(observed["output_video_seconds_ms"].is_null(), "{observed}");
    // Exactly the hold: within it.
    let (state, _) = settled_with(&f, &fake, &j, ReportedCost::Known(4_000_000)).await;
    assert_eq!(state, ("settled".into(), held, Some(4_000_000), false));
    // Unknown: the hold is kept, never a zero.
    let (state, evidence) = settled_with(&f, &fake, &j, ReportedCost::Unknown).await;
    assert_eq!(state, ("unknown".into(), held, None, false));
    assert_eq!(evidence, None);
    // Zero on a paid route proves nothing: unknown.
    let (state, _) = settled_with(&f, &fake, &j, ReportedCost::Known(0)).await;
    assert_eq!(state, ("unknown".into(), held, None, false));
    // Above the hold: settled as reported and marked unbounded, as usage
    // above a price's ceiling is.
    let (state, evidence) = settled_with(&f, &fake, &j, ReportedCost::Known(5_000_000)).await;
    assert_eq!(state, ("settled".into(), held, Some(5_000_000), true));
    assert_eq!(evidence.as_deref(), Some("provider_reported_cost"));
    // A free route: a zero figure settles as zero.
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.push(line(Meter::OutputVideoSecondsMs, "0", 1000));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units,created_at) VALUES($1,$2,0,1,3,$3,'{}',clock_timestamp())")
        .bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
    let (state, _) = settled_with(&f, &fake, &j, ReportedCost::Known(0)).await;
    assert_eq!(state, ("settled".into(), Some(0), Some(0), false));
    // A cost on that free route: settled as reported, above its zero hold.
    let (state, _) = settled_with(&f, &fake, &j, ReportedCost::Known(1)).await;
    assert_eq!(state, ("settled".into(), Some(0), Some(1), true));
}

/// Only a video's settlement may say the charge is the provider's figure: a
/// usage claiming it for any other workload, on a price that values it, is
/// not valued (the same usage without the claim settles).
#[dbtest::test]
async fn a_provider_figure_counts_only_for_a_video(pool: PgPool) {
    let f = fixture(pool).await;
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| ![Meter::InputTokens, Meter::OutputTokens].contains(m))
        .map(na)
        .collect();
    lines.push(line(Meter::InputTokens, "1000000", 1_000_000));
    lines.push(line(Meter::OutputTokens, "1000000", 1_000_000));
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units,created_at) VALUES($1,$2,100000,100000,3,$3,'{}',clock_timestamp())")
        .bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
    let finished = async |claims: bool| {
        let start = f.start();
        let admitted = crate::governance::admit_for_deployment(
            &f.store,
            &start,
            &crate::governance::tests::db::request(),
            30,
            &f.store
                .deployments(&f.principal, "company/smart")
                .await
                .unwrap()
                .remove(0),
        )
        .await;
        assert!(admitted.is_ok(), "{admitted:?}");
        let finish = crate::inference::repository::ExecutionFinish {
            id: start.id,
            outcome: Outcome::Succeeded,
            error: None,
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(10),
                billing: Some(crate::billing::BillingUsage {
                    total_input_tokens: Some(10),
                    uncached_input_tokens: Some(10),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                provider_cost_microusd: Some(1),
                provider_cost_is_charge: claims,
                ..Usage::default()
            },
            elapsed_ms: 1,
        };
        crate::governance::finish(&f.store, &finish).await.unwrap();
        let (state, _, actual, _) = amounts(&f, start.id).await;
        (state, actual)
    };
    assert_eq!(finished(false).await, ("settled".into(), Some(20)));
    assert_eq!(finished(true).await, ("unknown".into(), None));
}

/// A new latest price for the fixture's route: tokens as `tokens`, no
/// request fee, these video-seconds lines, and these unit ceilings.
async fn price(f: &Fixture, tokens: Option<Value>, video: Vec<Value>, max_units: Value) {
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests && (tokens.is_none() || *m != Meter::OutputTokens))
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    lines.extend(tokens);
    lines.extend(video);
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units,created_at) VALUES($1,$2,0,1,3,$3,$4,clock_timestamp())")
        .bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).bind(max_units).execute(&f.store.pool).await.unwrap();
}
fn video_line(rate: &str, size: Option<&str>) -> Value {
    let mut l = line(Meter::OutputVideoSecondsMs, rate, 1000);
    if let Some(size) = size {
        l["variant"] = json!(size);
    }
    l
}

/// Whether a route charges anything covers every rate the admitted video
/// may be charged at; a price that prices tokens leaves a video unknown (it
/// reports none); and a price that bounds no hold is already unbounded.
#[dbtest::test]
async fn the_route_s_rates_decide_a_zero_and_a_figure(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('personal',20) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs")
        .execute(&f.store.pool).await.unwrap();
    let fake = Fake::default();
    let j = jobs(&f, &fake, true).await;
    let sizes = ["720x1280", "1280x720", "1024x1792", "1792x1024"];
    // Every listed size paid, the line without a size free; a request that
    // names no size may have been made at a paid size: a zero is unknown.
    let mut video = vec![video_line("0", None)];
    video.extend(sizes.iter().map(|s| video_line("100000", Some(s))));
    price(&f, None, video, json!({})).await;
    let unnamed = |cost| {
        fake.push_video(upstream_video(JobState::Queued, None));
        let mut done = upstream_video(JobState::Completed, None);
        done.cost = Some(cost);
        (
            done,
            VideoRequest {
                size: None,
                ..video_request()
            },
        )
    };
    let settle = async |cost| {
        let (done, request) = unnamed(cost);
        let id = Uuid::new_v4();
        j.create_video(f.principal, request, id).await.unwrap();
        fake.push_video(done);
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(&f.store.pool)
            .await
            .unwrap();
        let polled = j.poll_once().await.unwrap();
        let rows: Vec<(String, Option<String>, bool, bool)> = sqlx::query_as("SELECT state,error_code,settled_at IS NOT NULL,output_pending FROM async_jobs ORDER BY created_at").fetch_all(&f.store.pool).await.unwrap();
        assert_eq!(polled, 1, "{rows:?}");
        amounts(&f, id).await
    };
    let (state, _, actual, _) = settle(ReportedCost::Known(0)).await;
    assert_eq!((state.as_str(), actual), ("unknown", None), "a paid size");
    // A figure within the hold settles, not unbounded, though the rates
    // alone cannot value a request that named no size.
    let (state, _, actual, unbounded) = settle(ReportedCost::Known(300_000)).await;
    assert_eq!(
        (state.as_str(), actual, unbounded),
        ("settled", Some(300_000), false),
        "no size named"
    );
    // Every listed size free and no line without a size: a zero settles.
    price(
        &f,
        None,
        sizes.iter().map(|s| video_line("0", Some(s))).collect(),
        json!({}),
    )
    .await;
    let (state, _, actual, _) = settle(ReportedCost::Known(0)).await;
    assert_eq!(
        (state.as_str(), actual),
        ("settled", Some(0)),
        "every size free"
    );
    // A priced output-token line: a video reports no tokens, so it stays
    // unknown with its hold (as on every video route), never retried as a
    // settlement the row cannot hold.
    price(
        &f,
        Some(line(Meter::OutputTokens, "1000000", 1_000_000)),
        vec![video_line("100000", None)],
        json!({}),
    )
    .await;
    let (state, _, actual, _) = settle(ReportedCost::Known(700_000)).await;
    assert_eq!((state.as_str(), actual), ("unknown", None), "token lines");
    // A video ceiling below the admitted length: no finite hold (no budget
    // applies here), so the figure settles, marked unbounded.
    price(
        &f,
        None,
        vec![video_line("100000", None)],
        json!({"output_video_seconds_ms": "1000"}),
    )
    .await;
    let (state, held, actual, unbounded) = settle(ReportedCost::Known(700_000)).await;
    assert_eq!(
        (state.as_str(), actual, unbounded),
        ("settled", Some(700_000), true),
        "held {held:?}"
    );
    // Rates too large to value, by size, and no size named: they charge,
    // and the figure settles (no finite hold), never a failed settlement.
    price(
        &f,
        None,
        sizes
            .iter()
            .map(|s| video_line("2000000000000000000", Some(s)))
            .collect(),
        json!({"output_video_seconds_ms": "1000"}),
    )
    .await;
    let (state, _, actual, unbounded) = settle(ReportedCost::Known(288_000)).await;
    assert_eq!(
        (state.as_str(), actual, unbounded),
        ("settled", Some(288_000), true),
        "rates beyond i64"
    );
    let (state, _, actual, _) = settle(ReportedCost::Known(0)).await;
    assert_eq!((state.as_str(), actual), ("unknown", None), "a zero there");
}

/// An operator resolves an unknown OpenRouter video as any video: with the
/// usage it records (the seconds, and the admitted size repeated), valued at
/// the route's rate for that size, never as a provider's figure (the
/// management handler always passes `provider_cost_is_charge: false`).
#[dbtest::test]
async fn an_operator_resolves_an_unknown_figure_at_the_route_s_rates(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true).await;
    fake.push_video(upstream_video(JobState::Queued, None));
    let request_id = Uuid::new_v4();
    j.create_video(f.principal, video_request(), request_id)
        .await
        .unwrap();
    let mut done = upstream_video(JobState::Completed, None);
    done.cost = Some(ReportedCost::Unknown);
    fake.push_video(done);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(amounts(&f, request_id).await.0, "unknown");
    let usage = Usage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        meters: Some(video_meters(Some(8000))),
        output_image_variant: crate::billing::MeterVariant::new("720x1280"),
        ..Usage::default()
    };
    crate::governance::resolve_usage(
        &f.store,
        f.principal.workspace_id,
        request_id,
        usage,
        "operator:receipt",
        f.owner,
    )
    .await
    .unwrap();
    // 8 s at the 720x1280 rate ($0.10/s), not a provider's figure.
    let (state, _, actual, _) = amounts(&f, request_id).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(800_000)));
    let figure: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND evidence='provider_reported_cost'",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(figure, 0);
}
