//! Custom patterns: compiled with fixed limits, so an administrator's pattern
//! cannot exhaust memory or time. A pattern is compiled at save; whatever later
//! compiles stored patterns uses `compile` too, so it holds the same limits. The `regex` crate matches in time linear in the input (no
//! backtracking), and these limits bound what a compiled pattern may hold.
use regex::{Regex, RegexBuilder};

/// The compiled program of one pattern (the crate's default is 10 MiB).
pub const COMPILED_LIMIT: usize = 1 << 20;
/// The lazy DFA cache of one pattern (the crate's default is 2 MiB).
pub const DFA_LIMIT: usize = 1 << 20;
/// How deeply a pattern may nest (the crate's default is 250).
pub const NEST_LIMIT: u32 = 64;

/// Compile one custom pattern within the limits.
pub fn compile(pattern: &str) -> Option<Regex> {
    // PostgreSQL text cannot hold NUL, so such a pattern is never storable.
    if pattern.is_empty()
        || pattern.len() > super::policy::MAX_PATTERN_BYTES
        || pattern.contains('\0')
    {
        return None;
    }
    RegexBuilder::new(pattern)
        .size_limit(COMPILED_LIMIT)
        .dfa_size_limit(DFA_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
        .ok()
}

/// Whether a pattern compiles within the limits (the save-time check).
pub fn valid(pattern: &str) -> bool {
    compile(pattern).is_some()
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
        // Small to write, enormous to compile: the size limit refuses it.
        assert!(!valid(r"\w{1000}\w{1000}"));
        assert!(
            !valid(&format!("{}a{}", "(".repeat(70), ")".repeat(70))),
            "nesting"
        );
    }
}
