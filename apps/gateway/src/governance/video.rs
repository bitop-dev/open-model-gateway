//! Video admission (#52): a video request is admitted only on a route whose
//! options (0097) take its length and size and whose price can value it.
//!
//! "Missing rates are not zero": a video route must price the video-seconds
//! meter (`output_video_seconds_ms`, pricing v3) for every size the request
//! may produce, as settlement values it. A request that names a size needs
//! a rate for that size or the line without a size; one that names none may
//! be served at any size, so the price must have one rate for every size (no
//! size-specific line) or name each size the route lists, and a route
//! without sizes needs that one rate. Anything else is refused before
//! anything is written, budgeted or not.
use super::*;
use crate::billing::v3::Meter;

/// What a video admission checks again under the shared catalog lock: the
/// resolved length and the named size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoCheck {
    pub seconds: u32,
    pub size: Option<String>,
}

/// Whether `lines` value video seconds at `size` (`None`: unreported) as
/// billing does (`billing::v3` `group` and `tier`): the meter has lines and
/// none is not-applicable; the size's own lines are used when it has any,
/// else the lines without a size; and they must be priced without a prompt
/// tier (a video reports no prompt tokens, so a tier could not be chosen).
fn size_rated(lines: &crate::billing::v3::PriceLines, size: Option<&str>) -> bool {
    let all: Vec<_> = lines
        .0
        .iter()
        .filter(|l| l.meter == Meter::OutputVideoSecondsMs)
        .collect();
    if all.is_empty() || all.iter().any(|l| l.rate.is_none()) {
        return false;
    }
    let of = |variant: Option<&str>| -> Vec<_> {
        all.iter()
            .filter(|l| l.variant.as_ref().map(|v| v.as_str()) == variant)
            .collect()
    };
    let group = match size.map(|s| of(Some(s))) {
        Some(specific) if !specific.is_empty() => specific,
        _ => of(None),
    };
    !group.is_empty() && group.iter().all(|l| l.min_prompt_tokens.is_none())
}
/// Whether `size` has lines of its own (billing then uses those, not the
/// lines without a size), priced as [`size_rated`] requires.
fn size_named(lines: &crate::billing::v3::PriceLines, size: &str) -> bool {
    lines.0.iter().any(|l| {
        l.meter == Meter::OutputVideoSecondsMs
            && l.variant.as_ref().is_some_and(|v| v.as_str() == size)
    }) && size_rated(lines, Some(size))
}

/// Whether `lines` have one video rate for every size: no size-specific
/// line, and the lines without a size rated ([`size_rated`]).
fn flat(lines: &crate::billing::v3::PriceLines) -> bool {
    !lines
        .0
        .iter()
        .any(|l| l.meter == Meter::OutputVideoSecondsMs && l.variant.is_some())
        && size_rated(lines, None)
}

/// Whether `price` values a video request naming `size` (or none) on a
/// route accepting `sizes`, as settlement will bill it
/// (`jobs::video::settlement`): a named size at its own lines or the lines
/// without a size; with no size named, the size the server reports only
/// where the price names it or rates every size alike, so each size the
/// route lists must be named by the price, unless the price is flat, and a
/// route without sizes needs a flat price.
pub(super) fn video_priced(
    price: Option<&Price>,
    sizes: Option<&[String]>,
    size: Option<&str>,
) -> bool {
    let Some(price) = price.filter(|p| p.pricing_version == 3) else {
        return false;
    };
    let Ok((lines, _)) = price.lines() else {
        return false;
    };
    match (size, sizes) {
        (Some(s), _) => size_rated(&lines, Some(s)),
        (None, Some(sizes)) => flat(&lines) || sizes.iter().all(|s| size_named(&lines, s)),
        (None, None) => flat(&lines),
    }
}

/// How the price pinned at a video's admission values a size the provider
/// reports for a request that named none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportedSizeRate {
    /// The price names that size itself: billed at its rate.
    Named,
    /// The price has no size-specific video line at all (one rate for every
    /// size): billed at that rate, whatever size is reported.
    Flat,
    /// Neither: the cost stays unknown and the hold is kept.
    Unpriced,
}

/// For a video whose request named no size: how the price version pinned at
/// its admission values the reported `size` (`None`: the server reported
/// none, which only a flat price values). The price is fixed at admission,
/// so a later change of the route's sizes never changes it.
pub async fn pinned_video_size_rate(
    store: &Store,
    execution: Uuid,
    size: Option<&str>,
) -> Result<ReportedSizeRate, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    let pinned: Option<(Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT price_id,deployment_id FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(execution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(storage)?;
    let rate = match pinned {
        Some((Some(id), deployment)) => {
            let price = price_version(&mut tx, id, deployment).await?;
            match price.lines() {
                Ok((lines, _)) if price.pricing_version == 3 => reported_rate(&lines, size),
                _ => ReportedSizeRate::Unpriced,
            }
        }
        _ => ReportedSizeRate::Unpriced,
    };
    tx.commit().await.map_err(storage)?;
    Ok(rate)
}
fn reported_rate(lines: &crate::billing::v3::PriceLines, size: Option<&str>) -> ReportedSizeRate {
    if size.is_some_and(|s| size_named(lines, s)) {
        ReportedSizeRate::Named
    } else if flat(lines) {
        ReportedSizeRate::Flat
    } else {
        ReportedSizeRate::Unpriced
    }
}

/// The route's video options now: `None` when it serves no video.
async fn options<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    deployment: Uuid,
) -> Result<Option<(Vec<i32>, Option<Vec<String>>)>, InferenceError> {
    type Options = (Option<Vec<i32>>, Option<Vec<String>>);
    let row: Option<Options> =
        sqlx::query_as("SELECT video_seconds,video_sizes FROM deployments WHERE id=$1")
            .bind(deployment)
            .fetch_optional(executor)
            .await
            .map_err(storage)?;
    Ok(row.and_then(|(seconds, sizes)| seconds.map(|s| (s, sizes))))
}

/// Inside the admission transaction, under the shared catalog lock (so the
/// options and the latest price cannot change before commit): the route
/// still takes `check`, and `price` values it. A route changed since it was
/// chosen is refused (`Unsupported`, which the caller classifies again as
/// before admission), and so is one whose price cannot value the request
/// (`PriceUnbounded`, answered as before admission), before anything is
/// written.
pub(super) async fn admit_check(
    tx: &mut Tx<'_>,
    deployment: Uuid,
    price: Option<&Price>,
    check: &VideoCheck,
) -> Result<(), InferenceError> {
    let (seconds, sizes) = options(&mut **tx, deployment)
        .await?
        .ok_or(InferenceError::Unsupported)?;
    let takes_length = i32::try_from(check.seconds).is_ok_and(|s| seconds.contains(&s));
    let takes_size = check
        .size
        .as_ref()
        .is_none_or(|s| sizes.as_ref().is_some_and(|sizes| sizes.contains(s)));
    if !takes_length || !takes_size {
        return Err(InferenceError::Unsupported);
    }
    if !video_priced(price, sizes.as_deref(), check.size.as_deref()) {
        return Err(InferenceError::PriceUnbounded);
    }
    Ok(())
}

/// Before admission (no lock; [`admit_check`] repeats it under the lock):
/// whether the route's latest price values a request naming `size` on it.
pub async fn video_route_priced(
    store: &Store,
    deployment: Uuid,
    size: Option<&str>,
) -> Result<bool, InferenceError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(begin_storage)?;
    let price = latest_price(&mut tx, deployment).await?;
    let sizes = options(&mut *tx, deployment).await?.and_then(|(_, s)| s);
    tx.commit().await.map_err(storage)?;
    Ok(video_priced(price.as_ref(), sizes.as_deref(), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(lines: serde_json::Value) -> Price {
        Price {
            id: Uuid::nil(),
            input_microusd_per_million: None,
            output_microusd_per_million: None,
            input_token_limit: 0,
            output_token_limit: 0,
            pricing_version: 3,
            cache_pricing: None,
            price_lines: Some(lines),
            max_units: Some(serde_json::json!({"output_video_seconds_ms": "3600000"})),
            batch_price_lines: None,
        }
    }
    fn line(variant: Option<&str>) -> serde_json::Value {
        let m = Meter::OutputVideoSecondsMs;
        let mut l = serde_json::json!({"meter":m.as_str(),"microusd_per_batch":"100","batch":1000,"unit_label":m.unit_label(1000).unwrap(),"sku_label":"Video"});
        if let Some(v) = variant {
            l["variant"] = serde_json::json!(v);
        }
        l
    }

    #[test]
    fn a_video_request_needs_a_rate_for_every_size_it_may_produce() {
        let sizes = vec!["1280x720".to_owned(), "720x1280".to_owned()];
        let default = price(serde_json::json!([line(None)]));
        let one = price(serde_json::json!([line(Some("1280x720"))]));
        assert!(video_priced(Some(&default), None, None));
        assert!(video_priced(Some(&default), Some(&sizes), None));
        assert!(video_priced(Some(&default), Some(&sizes), Some("720x1280")));
        assert!(video_priced(Some(&one), Some(&sizes), Some("1280x720")));
        assert!(!video_priced(Some(&one), Some(&sizes), Some("720x1280")));
        assert!(
            !video_priced(Some(&one), Some(&sizes), None),
            "720x1280 unpriced"
        );
        assert!(
            !video_priced(Some(&one), None, None),
            "no line without a size"
        );
        assert!(!video_priced(None, None, None), "no price");
        // A tier the video cannot select (no prompt tokens), or a
        // not-applicable line, values nothing.
        let mut tiered = line(Some("1280x720"));
        tiered["min_prompt_tokens"] = serde_json::json!(1000);
        let tiered = price(serde_json::json!([line(None), tiered]));
        assert!(!video_priced(Some(&tiered), Some(&sizes), Some("1280x720")));
        assert!(
            video_priced(Some(&tiered), None, Some("720x1280")),
            "the line without a size"
        );
        let na =
            price(serde_json::json!([{"meter":"output_video_seconds_ms","not_applicable":true}]));
        assert!(!video_priced(Some(&na), None, None));
        let (lines, _) = one.lines().unwrap();
        assert!(size_named(&lines, "1280x720"));
        assert_eq!(
            reported_rate(&lines, Some("1280x720")),
            ReportedSizeRate::Named
        );
        assert_eq!(
            reported_rate(&lines, Some("720x1280")),
            ReportedSizeRate::Unpriced
        );
        let (flat_lines, _) = default.lines().unwrap();
        assert_eq!(
            reported_rate(&flat_lines, Some("1280x720")),
            ReportedSizeRate::Flat,
            "one rate for any size"
        );
        let both = price(serde_json::json!([line(None), line(Some("1280x720"))]));
        let (lines, _) = both.lines().unwrap();
        assert_eq!(
            reported_rate(&lines, Some("1920x1080")),
            ReportedSizeRate::Unpriced,
            "never the fallback"
        );
        assert_eq!(
            reported_rate(&lines, None),
            ReportedSizeRate::Unpriced,
            "no size reported: only a flat price values it"
        );
        assert_eq!(reported_rate(&flat_lines, None), ReportedSizeRate::Flat);
        let (lines, _) = default.lines().unwrap();
        assert!(
            !size_named(&lines, "1280x720"),
            "only the line without a size"
        );
        // With no size named, admission takes only what settlement bills:
        // each listed size named by the price, or one rate for all.
        let other = vec!["640x360".to_owned()];
        let listed = vec!["1280x720".to_owned()];
        assert!(
            !video_priced(Some(&both), Some(&other), None),
            "640x360 only by the fallback"
        );
        assert!(video_priced(Some(&both), Some(&listed), None));
        assert!(
            !video_priced(Some(&both), None, None),
            "a route without sizes needs one rate"
        );
        assert!(
            video_priced(Some(&both), Some(&other), Some("640x360")),
            "a named size takes the fallback"
        );
        let mut v2 = default.clone();
        v2.pricing_version = 2;
        assert!(!video_priced(Some(&v2), None, None), "not a v3 price");
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod db {
    use super::*;
    use crate::{
        governance::tests::db::fixture,
        inference::{
            repository::InferenceRepository,
            types::WorkloadKind,
            workload::{OutputReservation, WorkloadAdmission},
        },
    };
    use sqlx::PgPool;

    /// Under the admission lock the route must still take the request and
    /// its price value it: a route changed since it was chosen is refused
    /// (`Unsupported`), an unpriced one too (`PriceUnbounded`), and nothing
    /// is written for either.
    #[dbtest::test]
    async fn the_locked_admission_checks_the_route_s_options_and_price_again(pool: PgPool) {
        let f = fixture(pool).await;
        crate::jobs::db_tests::video_setup(&f).await;
        let deployment = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        let admission = WorkloadAdmission {
            kind: WorkloadKind::Videos,
            output: OutputReservation::None,
            unit_ceilings: crate::jobs::unit_ceilings(1, 8000),
        };
        let admit = async |check: VideoCheck| {
            let start = f.start();
            let admitted = crate::governance::admit_video_for_deployment(
                &f.store,
                &start,
                &admission,
                60,
                &deployment,
                &check,
            )
            .await;
            let rows: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM governance_reservations WHERE execution_id=$1",
            )
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
            (admitted, rows)
        };
        let ok = VideoCheck {
            seconds: 8,
            size: Some("720x1280".into()),
        };
        assert_eq!(admit(ok.clone()).await, (Ok(()), 1));
        for check in [
            VideoCheck {
                seconds: 6,
                ..ok.clone()
            },
            VideoCheck {
                size: Some("640x360".into()),
                ..ok.clone()
            },
        ] {
            assert_eq!(admit(check).await, (Err(InferenceError::Unsupported), 0));
        }
        // A later price without a video rate: refused under the lock.
        use crate::jobs::db_tests::{line, na};
        let mut lines: Vec<serde_json::Value> = Meter::ALL
            .into_iter()
            .filter(|m| *m != Meter::Requests)
            .map(na)
            .collect();
        lines.push(line(Meter::Requests, "0", 1));
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
            .bind(Uuid::new_v4())
            .bind(f.deployment)
            .bind(serde_json::json!(lines))
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit(ok).await, (Err(InferenceError::PriceUnbounded), 0));
    }

    /// Readiness (`deployment_video::NOT_SET_UP`, SQL) and admission
    /// (`video_priced`) agree on every price shape: a video route counts as
    /// set up exactly when some request on it would be admitted (with no
    /// size named, or naming one of its sizes).
    #[dbtest::test]
    async fn readiness_and_admission_agree_on_every_price_shape(pool: PgPool) {
        use crate::jobs::db_tests::{line, na};
        use serde_json::{Value, json};
        let f = fixture(pool.clone()).await;
        crate::jobs::db_tests::video_setup(&f).await;
        let (model, connection): (Uuid, Uuid) =
            sqlx::query_as("SELECT model_id,provider_connection_id FROM deployments WHERE id=$1")
                .bind(f.deployment)
                .fetch_one(&pool)
                .await
                .unwrap();
        let video = |variant: Option<&str>, tier: Option<u64>| {
            let mut l = line(Meter::OutputVideoSecondsMs, "100000", 1000);
            if let Some(v) = variant {
                l["variant"] = json!(v);
            }
            if let Some(t) = tier {
                l["min_prompt_tokens"] = json!(t);
            }
            l
        };
        let (a, b) = ("1280x720", "720x1280");
        // (label, pricing version, video lines); `None`: no price at all.
        type PriceShape<'a> = (&'a str, Option<(i16, Vec<Value>)>);
        type RouteShape<'a> = (&'a str, Option<Vec<i32>>, Option<Vec<&'a str>>);
        let prices: Vec<PriceShape> = vec![
            ("no price", None),
            ("v1", Some((1, vec![]))),
            ("tokens only", Some((3, vec![]))),
            (
                "not applicable",
                Some((3, vec![na(Meter::OutputVideoSecondsMs)])),
            ),
            ("flat", Some((3, vec![video(None, None)]))),
            ("sized only", Some((3, vec![video(Some(a), None)]))),
            (
                "every listed size",
                Some((3, vec![video(Some(a), None), video(Some(b), None)])),
            ),
            (
                "sized and flat",
                Some((3, vec![video(None, None), video(Some(a), None)])),
            ),
            ("tiered flat", Some((3, vec![video(None, Some(1000))]))),
            ("tiered size", Some((3, vec![video(Some(a), Some(1000))]))),
            (
                "tiered fallback",
                Some((3, vec![video(None, Some(1000)), video(Some(a), None)])),
            ),
            (
                "flat and a tier",
                Some((3, vec![video(None, None), video(None, Some(1000))])),
            ),
        ];
        let shapes: [RouteShape; 6] = [
            ("no options", None, None),
            ("no sizes", Some(vec![4]), None),
            ("its size", Some(vec![4]), Some(vec![a])),
            ("both sizes", Some(vec![4]), Some(vec![a, b])),
            ("another size", Some(vec![4]), Some(vec!["640x360"])),
            ("one priced of two", Some(vec![4]), Some(vec!["640x360", a])),
        ];
        let mut checked = 0;
        for (label, price) in &prices {
            for (shape, seconds, sizes) in &shapes {
                let route = Uuid::new_v4();
                sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,video_seconds,video_sizes) VALUES($1,$2,$3,'clips',true,$4,$5)")
                    .bind(route).bind(model).bind(connection).bind(seconds).bind(sizes)
                    .execute(&pool).await.unwrap();
                match price {
                    None => {}
                    Some((1, _)) => {
                        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,50,1)")
                            .bind(Uuid::new_v4()).bind(route).execute(&pool).await.unwrap();
                    }
                    Some((_, video_lines)) => {
                        let mut lines: Vec<Value> = Meter::ALL
                            .into_iter()
                            .filter(|m| *m != Meter::Requests)
                            .map(na)
                            .collect();
                        lines.push(line(Meter::Requests, "0", 1));
                        lines.extend(video_lines.iter().cloned());
                        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')")
                            .bind(Uuid::new_v4()).bind(route).bind(json!(lines))
                            .execute(&pool).await.unwrap();
                    }
                }
                let not_set_up: bool = sqlx::query_scalar(&format!(
                    "SELECT {} FROM deployments d WHERE d.id=$1",
                    crate::management::deployment_video::NOT_SET_UP
                ))
                .bind(route)
                .fetch_one(&pool)
                .await
                .unwrap();
                let mut tx = pool.begin().await.unwrap();
                let latest = latest_price(&mut tx, route).await.unwrap();
                let opts = options(&mut *tx, route).await.unwrap();
                tx.commit().await.unwrap();
                let admitted = opts.is_some_and(|(_, sizes)| {
                    let sizes = sizes.as_deref();
                    video_priced(latest.as_ref(), sizes, None)
                        || sizes
                            .into_iter()
                            .flatten()
                            .any(|s| video_priced(latest.as_ref(), sizes, Some(s)))
                });
                assert_eq!(not_set_up, !admitted, "{label}, {shape}");
                checked += 1;
            }
        }
        assert_eq!(checked, prices.len() * shapes.len());
    }
}
