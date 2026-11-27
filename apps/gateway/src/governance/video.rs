//! Video admission (#52): a video request is admitted only on a route whose
//! options (0097) take its length and size and whose price can value it.
//!
//! "Missing rates are not zero": a video route must price the video-seconds
//! meter (`output_video_seconds_ms`, pricing v3) for every size the request
//! may produce. A request that names a size needs a rate for that size or
//! the line without a size; one that names none may be served at any of the
//! route's sizes, so each needs a rate or the line without a size, and a
//! route without sizes needs the line without a size. Anything else is
//! refused before anything is written, budgeted or not.
use super::*;
use crate::billing::v3::Meter;

/// What a video admission checks again under the shared catalog lock: the
/// resolved length and the named size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoCheck {
    pub seconds: u32,
    pub size: Option<String>,
}

/// Whether `price` values a video request naming `size` (or none) on a
/// route accepting `sizes`.
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
    let rated = |variant: Option<&str>| {
        lines.0.iter().any(|l| {
            l.meter == Meter::OutputVideoSecondsMs
                && l.rate.is_some()
                && l.min_prompt_tokens.is_none()
                && l.variant.as_ref().map(|v| v.as_str()) == variant
        })
    };
    let covered = |s: &str| rated(Some(s)) || rated(None);
    match (size, sizes) {
        (Some(s), _) => covered(s),
        (None, Some(sizes)) => sizes.iter().all(|s| covered(s)),
        (None, None) => rated(None),
    }
}

/// The route's video options now: `None` when it serves no video.
async fn options<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    deployment: Uuid,
) -> Result<Option<(Vec<i32>, Option<Vec<String>>)>, InferenceError> {
    let row: Option<(Option<Vec<i32>>, Option<Vec<String>>)> =
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
/// chosen is refused (`Unsupported`), and so is one that cannot be priced
/// (`Configuration`), before anything is written.
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
        return Err(InferenceError::Configuration);
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
        let mut v2 = default.clone();
        v2.pricing_version = 2;
        assert!(!video_priced(Some(&v2), None, None), "not a v3 price");
    }
}
