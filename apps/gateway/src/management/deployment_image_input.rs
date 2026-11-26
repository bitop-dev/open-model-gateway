//! A route's PATCH: its enablement, through the typed enablement operation
//! (revision and history, 0067), and its image-input opt-in (0094, #24),
//! which is audited but is not enablement.
use super::*;
use resources::{
    Enabled, audit, catalog_tx, configuration_actor, platform_role_snapshot,
    require_platform_admin, toggle,
};

/// A route's update: its enablement (with its revision and history, 0067),
/// its image-input opt-in (0094), or both, in one transaction.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeploymentUpdate {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    image_input: Option<bool>,
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
        } => toggle(s, u, id, Enabled { enabled }, "deployments").await,
        DeploymentUpdate {
            enabled,
            image_input: Some(image_input),
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
            set_image_input(&mut tx, &u, id, image_input).await?;
            tx.commit().await?;
            Ok(ok())
        }
        DeploymentUpdate {
            enabled: None,
            image_input: None,
        } => Err(invalid().into()),
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
