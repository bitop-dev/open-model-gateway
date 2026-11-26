//! Custom patterns: compiled with fixed limits, so a pattern cannot exhaust
//! memory or time. A pattern is compiled at save; whatever later compiles
//! stored patterns uses `compile` too, so it holds the same limits. The
//! `regex` crate matches in time linear in the input (no backtracking); these
//! limits bound what a compiled pattern may hold, and so the work per byte.
use regex::{Regex, RegexBuilder};

/// Who wrote a pattern, which sets its program limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// A platform layer (Platform Admins): up to 1 MiB, so Unicode classes
    /// (`\w`, `\d`, `\b`) fit.
    Platform,
    /// A local layer, which any user writes for a personal workspace: up to
    /// 64 KiB, which also bounds the work per scanned byte when a pattern
    /// defeats the lazy DFA (ASCII classes such as `[0-9]` or `(?-u:\w)` stay
    /// small).
    Local,
}
impl Limit {
    /// The compiled program of one pattern (the crate's default is 10 MiB).
    pub fn program(self) -> usize {
        match self {
            Self::Platform => 1 << 20,
            Self::Local => 64 << 10,
        }
    }
    pub fn of(layer: super::policy::Layer) -> Self {
        if layer == super::policy::Layer::WorkspaceLocal {
            Self::Local
        } else {
            Self::Platform
        }
    }
}
/// The lazy DFA cache of one pattern (the crate's default is 2 MiB).
pub const DFA_LIMIT: usize = 1 << 20;
/// How deeply a pattern may nest (the crate's default is 250).
pub const NEST_LIMIT: u32 = 64;

/// Pattern compiles per replica (at save, and when the engine compiles a
/// workspace's policy), in separate slots for the platform layers (Platform
/// Admins) and the local layers (every User owns one, on their personal
/// workspace), so local writers never queue ahead of the platform.
pub static PLATFORM_COMPILES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
pub static LOCAL_COMPILES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Run `work` on the blocking pool while holding one of `slots`. The slot
/// moves into the blocking task, so it is held until the work ends even when
/// the caller is dropped (a client that disconnects never frees a slot while
/// its compile still runs). `None` if the work could not run.
pub async fn in_slot<T: Send + 'static>(
    slots: &'static tokio::sync::Semaphore,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let slot = slots.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .ok()
}

/// Compile one custom pattern within the limits of a local layer.
pub fn compile(pattern: &str) -> Option<Regex> {
    compile_within(pattern, Limit::Local)
}

/// Compile one custom pattern within `limit`. The byte limit is checked
/// before anything parses the pattern, so parsing is bounded too.
pub fn compile_within(pattern: &str, limit: Limit) -> Option<Regex> {
    // PostgreSQL text cannot hold NUL, so such a pattern is never storable.
    if pattern.is_empty()
        || pattern.len() > super::policy::MAX_PATTERN_BYTES
        || pattern.contains('\0')
    {
        return None;
    }
    let regex = RegexBuilder::new(pattern)
        .size_limit(limit.program())
        .dfa_size_limit(DFA_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
        .ok()?;
    // A pattern that can match the empty string matches at every position:
    // it finds nothing and costs a scan of every byte, so it is refused.
    let empty = regex_syntax::ParserBuilder::new()
        .nest_limit(NEST_LIMIT)
        .build()
        .parse(pattern)
        .ok()?
        .properties()
        .minimum_len()
        == Some(0);
    (!empty).then_some(regex)
}

/// Whether a pattern compiles within a local layer's limits.
pub fn valid(pattern: &str) -> bool {
    compile(pattern).is_some()
}

/// Whether a pattern compiles within `limit` (the save-time check).
pub fn valid_within(pattern: &str, limit: Limit) -> bool {
    compile_within(pattern, limit).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_patterns_compile() {
        for p in [
            r"\bPROJECT-[A-Z]{4}-\d{3}\b",
            r"(?i)internal\.example\.com",
            "codename (alpha|beta)",
        ] {
            assert!(valid(p), "{p}");
        }
    }

    /// The time the largest patterns that still compile take: run with
    /// `--ignored --nocapture` to measure it on a given machine.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn worst_case_compile_time() {
        for class in [r"\w", r"(?i)\p{L}", r"[\p{L}\p{N}]", r"(?i)[a-z]", "."] {
            // The largest repetition that compiles within the limits.
            let (mut low, mut high) = (1_u32, 4000_u32);
            while low < high {
                let mid = (low + high).div_ceil(2);
                if valid(&format!("{class}{{{mid}}}")) {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            let pattern = format!("{class}{{{low}}}");
            let started = std::time::Instant::now();
            let compiled = valid(&pattern);
            println!("{pattern}: compiles={compiled} in {:?}", started.elapsed());
        }
    }

    #[test]
    fn invalid_and_oversized_patterns_are_refused() {
        assert!(!valid(""));
        assert!(!valid("(unclosed"));
        assert!(!valid(&"a".repeat(513)));
        assert!(!valid("a\0b"), "NUL is not storable");
        // Refused by length before anything parses it.
        assert!(!valid(&"\\w".repeat(100_000)));
        for empty in ["x?", "(?:)", "\\b", "a*", "(?m)^", "a|"] {
            assert!(!valid(empty), "{empty} can match the empty string");
        }
        // Small to write, enormous to compile: the size limit refuses it.
        assert!(!valid(r"\w{1000}\w{1000}"));
        assert!(
            !valid(&format!("{}a{}", "(".repeat(70), ")".repeat(70))),
            "nesting"
        );
    }
}
