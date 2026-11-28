//! A video's provider id, recorded once per upstream account (#52).
use uuid::Uuid;

use super::{JOB_COLUMNS, JobRow, Result, job_settlement};
use crate::{jobs::types::UpstreamId, store::Store};

/// The upstream account of the job aliased `$j`, as SQL: the canonical base
/// URL of its connection (see [`record_video_upstream`]).
macro_rules! upstream_account {
    ($j:literal) => {
        concat!("(SELECT CASE WHEN p.provider='openai' THEN 'https://api.openai.com/v1' ELSE coalesce(nullif(rtrim(p.endpoint,'/'),''),'provider:'||p.provider) END FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=", $j, ".deployment_id)")
    };
}
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) use upstream_account;

/// Fill in the provider id of a video recorded before its create (#662).
/// `None` when the row is no longer a queued video without an id (the
/// create-grace rule already failed it), or when another job of the same
/// upstream account already has that id. The account is the server's
/// canonical base URL (an approved local base without its trailing
/// slashes, as `ApprovedEndpoints` compares it; OpenAI's fixed base),
/// whatever connection record or credential reaches it: two records may
/// name one server, and one server's ids are one namespace. One upstream object is
/// never reachable through two jobs (their routes may serve different
/// workspaces): the id is not recorded for this job, which ends `failed`
/// `upstream_id_reused` (hold kept), and every earlier job with that id is
/// marked `upstream_id_reused` (and ended `failed` if still running, its
/// hold kept), after which the gateway never contacts the server for it
/// again ([`super::Jobs::target`]); the server may have put another
/// workspace's video behind it. The check runs under an advisory
/// lock of (account, id), so two creates answered with one id cannot both
/// record it. The provider has accepted the job at this point, so a failure
/// is counted at `job_settlement`.
pub(crate) async fn record_video_upstream(
    store: &Store,
    id: Uuid,
    upstream: &UpstreamId,
) -> Result<Option<JobRow>> {
    const ACCOUNT: &str = upstream_account!("j");
    let mut tx = crate::db::begin(&store.pool)
        .await
        .map_err(job_settlement)?;
    sqlx::query(&format!("SELECT pg_advisory_xact_lock(hashtextextended('video_upstream:'||{ACCOUNT}||':'||$2,0)) FROM async_jobs j WHERE j.id=$1"))
        .bind(id).bind(upstream.as_str())
        .execute(&mut *tx).await.map_err(job_settlement)?;
    // Every job of the account holding the id, deleted ones too: a delete
    // asks the server to delete its copy after the job is marked deleted,
    // so an id given out again meanwhile would have that request remove the
    // new holder's video. A deleted job is not marked (`others_live`).
    let others: Vec<(Uuid, bool)> = sqlx::query_as(&format!("SELECT o.id,o.deleted_at IS NULL FROM async_jobs o JOIN async_jobs j ON j.id=$1 WHERE o.upstream_id=$2 AND o.id<>$1 AND o.kind='video' AND {} = {ACCOUNT}", upstream_account!("o")))
        .bind(id).bind(upstream.as_str())
        .fetch_all(&mut *tx).await.map_err(job_settlement)?;
    let others_live: Vec<Uuid> = others
        .iter()
        .filter(|(_, live)| *live)
        .map(|(id, _)| *id)
        .collect();
    let recorded: Option<JobRow> = if others.is_empty() {
        sqlx::query_as(&format!("UPDATE async_jobs SET upstream_id=$2 WHERE id=$1 AND kind='video' AND upstream_id IS NULL AND state='queued' RETURNING {JOB_COLUMNS}"))
            .bind(id).bind(upstream.as_str())
            .fetch_optional(&mut *tx).await.map_err(job_settlement)?
    } else {
        // Marked for good, and ended if still running (its hold is kept: no
        // settlement is ever read from the server for it).
        sqlx::query("UPDATE async_jobs SET error_code='upstream_id_reused',state=CASE WHEN state IN('queued','in_progress') THEN 'failed' ELSE state END,completed_at=CASE WHEN state IN('queued','in_progress') THEN coalesce(completed_at,clock_timestamp()) ELSE completed_at END WHERE id=ANY($1)")
            .bind(&others_live)
            .execute(&mut *tx).await.map_err(job_settlement)?;
        // This job too: the server's answer gives it no object of its own.
        sqlx::query("UPDATE async_jobs SET state='failed',upstream_status='failed',error_code='upstream_id_reused',completed_at=clock_timestamp() WHERE id=$1 AND kind='video' AND upstream_id IS NULL AND state='queued'")
            .bind(id)
            .execute(&mut *tx).await.map_err(job_settlement)?;
        None
    };
    tx.commit().await.map_err(job_settlement)?;
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    /// The lookup of an id among an account's video jobs has its own index
    /// (0101): by the id, over video jobs only.
    #[dbtest::test]
    async fn the_id_lookup_has_a_partial_index_of_video_jobs(pool: PgPool) {
        let definition: String =
            sqlx::query_scalar("SELECT pg_get_indexdef('async_jobs_video_upstream_id'::regclass)")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            definition,
            "CREATE INDEX async_jobs_video_upstream_id ON public.async_jobs USING btree (upstream_id) WHERE (kind = 'video'::text)"
        );
    }

    /// One server is one account, whatever connection record or credential
    /// name reaches it: the approved base with or without its trailing slash
    /// and under two credential names is one account; another base, or
    /// another provider without an endpoint, is another.
    #[dbtest::test]
    async fn an_account_is_its_server_whatever_record_or_credential_names_it(pool: PgPool) {
        let model = Uuid::new_v4();
        sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/clip',ARRAY['videos'])")
            .bind(model).execute(&pool).await.unwrap();
        let mut routes = Vec::new();
        for (provider, endpoint, credential) in [
            (
                "openai_compatible",
                Some("http://gpu.internal:8000/v1"),
                "none",
            ),
            (
                "openai_compatible",
                Some("http://gpu.internal:8000/v1/"),
                "env:CLIP_A",
            ),
            (
                "openai_compatible",
                Some("http://gpu.internal:8000/v1//"),
                "env:CLIP_B",
            ),
            ("vllm", Some("http://gpu.internal:8000/v1"), "none"),
            (
                "openai_compatible",
                Some("http://gpu.internal:9000/v1"),
                "none",
            ),
            ("openai", None, "env:OPENAI_API_KEY"),
            (
                "openai",
                Some("https://api.openai.com/v1/"),
                "env:OPENAI_OTHER",
            ),
        ] {
            let (connection, route) = (Uuid::new_v4(), Uuid::new_v4());
            sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,$2,$3,$4,$5,true)")
                .bind(connection).bind(format!("c{}", routes.len())).bind(provider).bind(credential).bind(endpoint)
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'clip',true)")
                .bind(route).bind(model).bind(connection).execute(&pool).await.unwrap();
            routes.push(route);
        }
        let accounts: Vec<String> = sqlx::query_scalar(concat!(
            "SELECT ",
            upstream_account!("j"),
            " FROM unnest($1::uuid[]) WITH ORDINALITY AS j(deployment_id,n) ORDER BY n"
        ))
        .bind(&routes)
        .fetch_all(&pool)
        .await
        .unwrap();
        let base = "http://gpu.internal:8000/v1";
        assert_eq!(
            accounts,
            [
                base,
                base,
                base,
                base,
                "http://gpu.internal:9000/v1",
                "https://api.openai.com/v1",
                "https://api.openai.com/v1"
            ]
        );
    }
}
