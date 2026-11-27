//! A route's PATCH: its enablement, through the typed enablement operation
//! (revision and history, 0067), its image-input opt-in (0094, #24) and its
//! video options (0097, #52), which are audited but are not enablement.
use super::*;
use resources::{
    Enabled, audit, catalog_tx, configuration_actor, platform_role_snapshot,
    require_platform_admin, toggle,
};

/// A route's update: its enablement (with its revision and history, 0067),
/// its image-input opt-in (0094), its video options (0097; `null` clears
/// them), or several, in one transaction. A closed object, or the existing
/// positional form `[enabled, image_input]` (at most two items: the video
/// options are set only in the object form).
pub(super) struct DeploymentUpdate {
    enabled: Option<bool>,
    image_input: Option<bool>,
    video: Option<Option<super::deployment_video::VideoOptions>>,
}
impl<'de> Deserialize<'de> for DeploymentUpdate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = DeploymentUpdate;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a route update object, or [enabled, image_input]")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let enabled = a.next_element::<Option<bool>>()?.flatten();
                let image_input = a.next_element::<Option<bool>>()?.flatten();
                if a.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::invalid_length(3, &self));
                }
                Ok(DeploymentUpdate {
                    enabled,
                    image_input,
                    video: None,
                })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut update = DeploymentUpdate {
                    enabled: None,
                    image_input: None,
                    video: None,
                };
                let mut seen = std::collections::BTreeSet::new();
                while let Some(key) = a.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    match key.as_str() {
                        "enabled" => update.enabled = a.next_value()?,
                        "image_input" => update.image_input = a.next_value()?,
                        // `null` is a value here: it clears the options.
                        "video" => update.video = Some(a.next_value()?),
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["enabled", "image_input", "video"],
                            ));
                        }
                    }
                }
                Ok(update)
            }
        }
        d.deserialize_struct(
            "DeploymentUpdate",
            &["enabled", "image_input", "video"],
            Visit,
        )
    }
}
pub(super) async fn update_deployment(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<DeploymentUpdate>,
) -> DetailedResult {
    match b {
        DeploymentUpdate {
            enabled: Some(enabled),
            image_input: None,
            video: None,
        } => toggle(s, u, id, Enabled { enabled }, "deployments").await,
        DeploymentUpdate {
            enabled: None,
            image_input: None,
            video: None,
        } => Err(invalid().into()),
        DeploymentUpdate {
            enabled,
            image_input,
            video,
        } => {
            let mut tx = catalog_tx(&s, &u, true).await?;
            configuration_actor(&mut tx, &u).await?;
            if let Some(enabled) = enabled {
                let change = super::deployment_configuration::DeploymentChange::prepare(
                    &mut tx,
                    &u,
                    id,
                    &Enabled { enabled },
                )
                .await?;
                change.apply(&mut tx).await?;
            } else {
                // Without an enablement change, `DeploymentChange::prepare`
                // (which checks it) does not run: check live authority here.
                require_platform_admin(&platform_role_snapshot(&mut tx, u.user_id()).await?)?;
            }
            if let Some(image_input) = image_input {
                set_image_input(&mut tx, &u, id, image_input).await?;
            }
            if let Some(video) = video {
                super::deployment_video::set_video_options(&mut tx, &u, id, video).await?;
            }
            tx.commit().await?;
            Ok(ok())
        }
    }
}
/// Records a route's image-input opt-in and audits a change. The caller
/// holds an exclusive `catalog_tx` and live Platform Admin authority.
async fn set_image_input(
    tx: &mut Transaction<'_, Postgres>,
    u: &ManagementPrincipal,
    id: Uuid,
    image_input: bool,
) -> Result<(), ManagementError> {
    let before: Option<bool> =
        sqlx::query_scalar("SELECT image_input FROM deployments WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(before) = before else {
        return Err(missing().into());
    };
    if before == image_input {
        return Ok(());
    }
    sqlx::query("UPDATE deployments SET image_input=$2 WHERE id=$1")
        .bind(id)
        .bind(image_input)
        .execute(&mut **tx)
        .await?;
    audit(
        tx,
        u,
        None,
        "deployment.image_input_changed",
        "deployment",
        Some(id),
        json!({"image_input": image_input}),
    )
    .await?;
    Ok(())
}
