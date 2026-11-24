//! The `POST /v1/messages/count_tokens` limits' startup parsing.
use anyhow::Context;

/// One `POST /v1/messages/count_tokens` limit (`inference::count`), read
/// with `lookup`: unset means `default`; otherwise the value must be ASCII
/// decimal digits only (leading zeros allowed, so `007` is 7; no sign,
/// spaces or other characters, so `+5` and ` 5` are refused) naming a
/// number from 1 to `maximum`, else startup fails naming the setting.
pub(super) fn count_limit(
    lookup: impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
    name: &str,
    default: u32,
    maximum: u32,
) -> anyhow::Result<u32> {
    match lookup(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{name} is not valid Unicode"),
        Ok(value) => Some(value.as_str())
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|n| (1..=maximum).contains(n))
            .with_context(|| format!("{name} must be an integer from 1 to {maximum}")),
    }
}

#[cfg(test)]
#[path = "config_count_limit_tests.rs"]
mod tests;
