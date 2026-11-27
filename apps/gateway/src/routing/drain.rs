//! An operator drain (#49): a drained deployment, or any deployment of a
//! drained provider connection, takes no new attempt. Its `drain_until`
//! is evaluated whenever routes are read, so the drain lifts on its own; no
//! job clears it. Everywhere else the route is as it was: enabled, listed,
//! catalog-eligible and selectable.
//!
//! Routing sees a drain through the per-replica health cache (at most
//! [`crate::cache::HEALTH_TTL`] old on another replica; the replica that set
//! it forgets its entry at once). Admission refuses a drained route under the
//! shared catalog lock that a drain change takes exclusively, so no attempt is
//! admitted once a drain has committed, whatever plan the request carries.

/// The route (deployment `d`, its connection `p`) is drained now.
pub(crate) const ROUTE_DRAINED: &str = "((d.drained_at IS NOT NULL AND (d.drain_until IS NULL OR d.drain_until > statement_timestamp())) OR (p.drained_at IS NOT NULL AND (p.drain_until IS NULL OR p.drain_until > statement_timestamp())))";

/// Seconds until a drained route's drain ends (both of its drains, where
/// both apply); NULL while it is not drained, or while either drain that
/// applies has no end.
pub(crate) const DRAIN_ENDS_IN: &str = "CASE WHEN ((d.drained_at IS NOT NULL AND (d.drain_until IS NULL OR d.drain_until > statement_timestamp())) OR (p.drained_at IS NOT NULL AND (p.drain_until IS NULL OR p.drain_until > statement_timestamp()))) AND NOT ((d.drained_at IS NOT NULL AND d.drain_until IS NULL) OR (p.drained_at IS NOT NULL AND p.drain_until IS NULL)) THEN EXTRACT(EPOCH FROM GREATEST(CASE WHEN d.drained_at IS NOT NULL AND d.drain_until > statement_timestamp() THEN d.drain_until END, CASE WHEN p.drained_at IS NOT NULL AND p.drain_until > statement_timestamp() THEN p.drain_until END) - statement_timestamp())::float8 END";

/// [`retry_after`] in SQL, for modules that hold no float (money code): the
/// whole seconds until a drain that ends within
/// [`DRAIN_RETRY_AFTER_WITHIN_SECONDS`], else NULL.
pub(crate) static DRAIN_RETRY_AFTER: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "(SELECT CASE WHEN e <= {DRAIN_RETRY_AFTER_WITHIN_SECONDS} THEN GREATEST(1, CEIL(e))::int END FROM (SELECT {DRAIN_ENDS_IN} AS e) ends)"
    )
});

/// A request to a model whose routes are all drained gets `Retry-After` only
/// when a drain ends within this many seconds; without an end, or with a far
/// one, it is told only that the model is temporarily unavailable.
pub const DRAIN_RETRY_AFTER_WITHIN_SECONDS: u32 = 3600;

/// Whole seconds until a drain that ends in `seconds` does, for a
/// `Retry-After`: `None` without an end or beyond
/// [`DRAIN_RETRY_AFTER_WITHIN_SECONDS`].
pub(crate) fn retry_after(seconds: Option<f64>) -> Option<u32> {
    seconds
        .filter(|s| s.is_finite())
        .map(|s| s.ceil().max(1.0))
        .filter(|s| *s <= f64::from(DRAIN_RETRY_AFTER_WITHIN_SECONDS))
        .map(|s| s as u32)
}
