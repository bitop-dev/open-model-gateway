//! The catalog transport: one fixed origin, bounded bodies, a bounded cache
//! and one upstream fetch at a time.
use super::*;

type Cache = HashMap<String, (Instant, Arc<Vec<u8>>)>;
pub(crate) struct OpenRouterCatalog {
    client: crate::egress::HttpClient,
    base: String,
    timeout: Duration,
    pub(super) cache: tokio::sync::Mutex<Cache>,
    /// Held for each upstream fetch, of any document, so at most one body is
    /// in flight. It guards when each document's latest fetch failed: a
    /// caller that waited while that fetch ran takes its failure instead of
    /// repeating it. The holder writes the entry with no await between the
    /// result and the write, so a cancelled fetch records nothing and the
    /// next waiter fetches for itself.
    fetch: tokio::sync::Mutex<Failures>,
}
type Failures = HashMap<String, (Instant, CatalogError)>;
/// A fetched document; `endpoints` is its projected endpoint list when this
/// read decoded it (a miss of an endpoints document), so a preview decodes
/// that document once.
struct Fetched {
    body: Arc<Vec<u8>>,
    endpoints: Option<limits::EndpointList>,
}
impl OpenRouterCatalog {
    fn build(base: String, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            // Every resolved address must pass the egress policy (#33).
            client: crate::egress::client_builder(crate::egress::resolver(
                crate::egress::Purpose::Catalog,
            ))
            .connect_timeout(timeout)
            .pool_idle_timeout(crate::egress::UpstreamClient::Openrouter.pool_idle_timeout())
            .build()
            .map_err(|_| anyhow::anyhow!("Unable to initialize catalog transport"))?,
            base,
            timeout,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            fetch: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
    pub(crate) fn production() -> anyhow::Result<Self> {
        Self::build(
            resources::OPENROUTER_BASE.to_owned(),
            Duration::from_secs(5),
        )
    }
    /// Test-only origin injection; restricted to a loopback mock server.
    #[cfg(test)]
    pub(crate) fn for_test(base: String, timeout: Duration) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        Self::build(base, timeout).unwrap()
    }
    async fn cached(&self, url: &str) -> Option<Arc<Vec<u8>>> {
        let cache = self.cache.lock().await;
        let (at, body) = cache.get(url)?;
        (at.elapsed() < CACHE_TTL).then(|| body.clone())
    }
    pub(super) async fn get(&self, path: &str) -> Result<Arc<Vec<u8>>, CatalogError> {
        self.read(path).await.map(|f| f.body)
    }
    /// The projected endpoint list of `/models/{slug}/endpoints`.
    pub(super) async fn endpoints(&self, slug: &str) -> Result<limits::EndpointList, CatalogError> {
        let fetched = self.read(&format!("/models/{slug}/endpoints")).await?;
        match fetched.endpoints {
            Some(list) => Ok(list),
            None => limits::endpoint_list_json(&fetched.body),
        }
    }
    /// One deadline bounds the whole read, waiting for another caller's
    /// fetch included, so callers queued behind a stalled catalog fail
    /// together instead of one timeout after another.
    async fn read(&self, path: &str) -> Result<Fetched, CatalogError> {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + self.timeout;
        let url = format!("{}{path}", self.base);
        let hit = |body| Fetched {
            body,
            endpoints: None,
        };
        if let Some(body) = self.cached(&url).await {
            return Ok(hit(body));
        }
        let mut failures = tokio::time::timeout_at(deadline, self.fetch.lock())
            .await
            .map_err(|_| CatalogError::Unavailable)?;
        if let Some(body) = self.cached(&url).await {
            return Ok(hit(body));
        }
        // A caller older than the timeout has given up, so older failures
        // cannot be shared and are dropped.
        failures.retain(|_, (at, _)| at.elapsed() < self.timeout);
        if let Some((at, error)) = failures.get(&url)
            && *at >= started
        {
            return Err(*error);
        }
        let result = self.fetch_upstream(&url, path, deadline).await;
        match &result {
            Err(error) => {
                if failures.len() >= CACHE_ENTRIES
                    && let Some(oldest) = failures
                        .iter()
                        .min_by_key(|(_, (at, _))| *at)
                        .map(|(k, _)| k.clone())
                {
                    failures.remove(&oldest);
                }
                failures.insert(url, (Instant::now(), *error));
            }
            Ok(_) => {
                failures.remove(&url);
            }
        }
        result
    }
    async fn fetch_upstream(
        &self,
        url: &str,
        path: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Fetched, CatalogError> {
        let body = tokio::time::timeout_at(deadline, async {
            let response = self
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(
                    crate::egress::UpstreamClient::Openrouter.count_then(CatalogError::Unavailable),
                )?;
            let json = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json"));
            // Redirects are not followed: any non-200 (including 3xx) fails.
            if response.status() != reqwest::StatusCode::OK || !json {
                return Err(CatalogError::Unavailable);
            }
            if response
                .content_length()
                .is_some_and(|n| n > BODY_CAP as u64)
            {
                return Err(CatalogError::Limit);
            }
            let mut out = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| CatalogError::Unavailable)?;
                if out.len() + chunk.len() > BODY_CAP {
                    return Err(CatalogError::Limit);
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
        .await
        .map_err(|_| CatalogError::Unavailable)??;
        // Decode only bounded, complete JSON; a malformed 200 must not poison
        // subsequent previews for the cache TTL.
        limits::catalog_json(&body)?;
        let body = Arc::new(body);
        let images = path.starts_with("/images/");
        let uncached = |body| {
            Ok(Fetched {
                body,
                endpoints: None,
            })
        };
        if images && !limits::mapping_json(&body) {
            // Image mapping applies its limits to the raw body and reports
            // the refusal; the response is not cached.
            return uncached(body);
        }
        // A refused projection is not cached, so a corrected response is
        // read on the next preview.
        let endpoints = (!images && path.ends_with("/endpoints"))
            .then(|| limits::endpoint_list_json(&body))
            .transpose()?;
        if images {
            let arrays = image_endpoint_arrays(&body);
            if arrays.iter().all(Option::is_none) {
                return Err(CatalogError::Unavailable);
            }
            if decode_image_endpoints(arrays).is_none() {
                // The envelope is valid, but these image prices cannot be read.
                // Let the mapper report the missing rates and retry on next use.
                return uncached(body);
            }
        } else if !catalog_shape_matches(path, &body) {
            return Err(CatalogError::Unavailable);
        }
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        // Oldest first until the new body fits; a body is at most BODY_CAP.
        while cache.len() >= CACHE_ENTRIES
            || cache.values().map(|(_, b)| b.len()).sum::<usize>() + body.len() > CACHE_BYTES
        {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            cache.remove(&oldest);
        }
        cache.insert(url.to_owned(), (Instant::now(), body.clone()));
        Ok(Fetched { body, endpoints })
    }
}
