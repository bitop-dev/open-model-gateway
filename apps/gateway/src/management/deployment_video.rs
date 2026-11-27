//! A route's video options (0097, #52): the clip lengths it accepts (the
//! first is used when a request names none), and the sizes it accepts. A
//! route without them serves no video request. Set by a Platform Admin
//! through the route's PATCH, under the exclusive catalog transaction, and
//! audited.
use super::*;
use crate::jobs::types::valid_video_size as valid_size;
use resources::audit;

pub(super) const OPTIONS_INVALID: &str = "Video options are 1 to 32 distinct clip lengths of 1 to 3600 seconds (the first is the default), and optionally 1 to 32 distinct sizes written WIDTHxHEIGHT (each 1 to 9999)";

/// A route's video options as the API reads and writes them: a closed
/// object (no positional form). Reading checks the shape only; the values
/// are checked by [`set_video_options`], which refuses them with `400`
/// `video_options_invalid`.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct VideoOptions {
    /// Clip lengths (seconds) the route accepts; the first is used when a
    /// request names none.
    pub seconds: Vec<i32>,
    /// Sizes the route accepts; absent or `null`: a request may not name a
    /// size, and none is sent upstream.
    pub sizes: Option<Vec<String>>,
}
impl<'de> Deserialize<'de> for VideoOptions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = VideoOptions;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object with `seconds` and optional `sizes`")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let (mut seconds, mut sizes) = (None, None);
                while let Some(key) = a.next_key::<String>()? {
                    match key.as_str() {
                        "seconds" if seconds.is_none() => seconds = Some(a.next_value()?),
                        "sizes" if sizes.is_none() => sizes = Some(a.next_value()?),
                        "seconds" | "sizes" => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate field `{key}`"
                            )));
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["seconds", "sizes"],
                            ));
                        }
                    }
                }
                Ok(VideoOptions {
                    seconds: seconds.ok_or_else(|| serde::de::Error::missing_field("seconds"))?,
                    sizes: sizes.flatten(),
                })
            }
        }
        // A struct with these fields: a map, never a sequence (no
        // `visit_seq`).
        d.deserialize_struct("VideoOptions", &["seconds", "sizes"], Visit)
    }
}

impl VideoOptions {
    fn validate(&self) -> Result<(), ManagementError> {
        if self.valid() {
            Ok(())
        } else {
            Err(ApiError(StatusCode::BAD_REQUEST, OPTIONS_INVALID).into())
        }
    }
    /// 1 to 32 distinct lengths of 1 to 3600 seconds, and optionally 1 to 32
    /// distinct `WIDTHxHEIGHT` sizes.
    pub(super) fn valid(&self) -> bool {
        let distinct = |n: usize, unique: usize| (1..=32).contains(&n) && unique == n;
        let seconds: std::collections::BTreeSet<_> = self.seconds.iter().collect();
        let ok_seconds = distinct(self.seconds.len(), seconds.len())
            && self.seconds.iter().all(|s| {
                u32::try_from(*s)
                    .is_ok_and(|s| (1..=crate::jobs::types::VIDEO_MAX_SECONDS).contains(&s))
            });
        let ok_sizes = self.sizes.as_ref().is_none_or(|sizes| {
            let unique: std::collections::BTreeSet<_> = sizes.iter().collect();
            distinct(sizes.len(), unique.len()) && sizes.iter().all(|s| valid_size(s))
        });
        ok_seconds && ok_sizes
    }
}

/// Records a route's video options (`None`: it serves no video) and audits
/// a change. The caller holds an exclusive `catalog_tx` and live Platform
/// Admin authority.
pub(super) async fn set_video_options(
    tx: &mut Transaction<'_, Postgres>,
    u: &ManagementPrincipal,
    id: Uuid,
    options: Option<VideoOptions>,
) -> Result<(), ManagementError> {
    if let Some(options) = &options {
        options.validate()?;
    }
    type Stored = (Option<Vec<i32>>, Option<Vec<String>>);
    let before: Option<Stored> =
        sqlx::query_as("SELECT video_seconds,video_sizes FROM deployments WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((seconds, sizes)) = before else {
        return Err(missing().into());
    };
    let current = seconds.map(|seconds| VideoOptions { seconds, sizes });
    if current == options {
        return Ok(());
    }
    sqlx::query("UPDATE deployments SET video_seconds=$2,video_sizes=$3 WHERE id=$1")
        .bind(id)
        .bind(options.as_ref().map(|o| o.seconds.clone()))
        .bind(options.as_ref().and_then(|o| o.sizes.clone()))
        .execute(&mut **tx)
        .await?;
    audit(
        tx,
        u,
        None,
        "deployment.video_options_changed",
        "deployment",
        Some(id),
        json!({"video": options}),
    )
    .await?;
    Ok(())
}

/// The audit allowance of `deployment.video_options_changed`: its `video`
/// field is `null` or validated options (numbers and `WIDTHxHEIGHT` sizes
/// only).
pub(super) fn audit_field(action: &str, key: &str, value: &Value) -> bool {
    action == "deployment.video_options_changed"
        && key == "video"
        && (value.is_null()
            || serde_json::from_value::<VideoOptions>(value.clone()).is_ok_and(|o| o.valid()))
}

/// The route's video options as the API renders them: `null` when it serves
/// no video.
pub(super) const VIDEO_JSON: &str = "CASE WHEN d.video_seconds IS NULL THEN NULL ELSE jsonb_build_object('seconds',to_jsonb(d.video_seconds),'sizes',to_jsonb(d.video_sizes)) END";

/// A route (`d`) of a video model that is not set up for video: no request
/// on it could be admitted. It needs video options and a latest price that
/// is pricing v3, states no video-seconds line not applicable or unknown,
/// and values some request as admission does (`governance::video`): one rate
/// for every size (no size-specific line, no prompt tier), or, on a route
/// with sizes, a listed size priced by its own lines or the lines without a
/// size (without a prompt tier). Such a route serves no video request, so
/// it is not counted as serving (`resources::SERVING_ROUTE`).
pub(super) const NOT_SET_UP: &str = "(EXISTS(SELECT 1 FROM models vm WHERE vm.id=d.model_id AND 'videos'=ANY(vm.supported_protocols)) AND NOT coalesce((SELECT d.video_seconds IS NOT NULL AND vp.pricing_version=3 AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND NOT l ? 'microusd_per_batch') AND (CASE WHEN d.video_sizes IS NULL THEN (NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l ? 'variant') AND EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms') AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l ? 'min_prompt_tokens')) ELSE (NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l ? 'variant') AND EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms') AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l ? 'min_prompt_tokens')) OR EXISTS(SELECT 1 FROM unnest(d.video_sizes) z WHERE (CASE WHEN EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l->>'variant'=z) THEN NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND l->>'variant'=z AND l ? 'min_prompt_tokens') ELSE EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND NOT l ? 'variant') AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(vp.price_lines) l WHERE l->>'meter'='output_video_seconds_ms' AND NOT l ? 'variant' AND l ? 'min_prompt_tokens') END)) END) FROM deployment_prices vp WHERE vp.deployment_id=d.id ORDER BY vp.created_at DESC,vp.id DESC LIMIT 1),false))";
