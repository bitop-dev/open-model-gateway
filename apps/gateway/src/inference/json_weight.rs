//! An upper bound on the structure a JSON body may parse into, computed from
//! its bytes before any parser runs (#872).
//!
//! A JSON value can be far larger parsed than written: `0,` is two bytes and
//! a `serde_json::Value` of at least 32, and a one-key object such as
//! `{"a":0}` (eight bytes) allocates a whole B-tree node of about 630. A
//! 2 MiB body of such values parsed into 180 MiB, before any validation
//! refused it and before admission bounds how many requests run at once.
//! [`walk`] reads the body once, with no allocation, and returns the structure
//! estimate: together with the body's own length it is never below what the
//! gateway's parse of the body allocates (`serde_json::from_slice`, into a
//! `Value` or a request type, as `http::json_body` parses; proved by the
//! property test `tests/json_weight.rs`, which measures the parse's
//! allocations). One thing is left out: the error message of a body that is
//! refused can hold one of its strings, decoded and escaped for display, for
//! a moment (at most about eight times that string, never sent); it is
//! bounded by the body, and charging it for every string would refuse a
//! legitimate long text. A body
//! whose structure estimate exceeds [`BUDGET`] is refused before it is
//! parsed.
//!
//! The body's length covers every string's bytes, which a parse never
//! lengthens (an escape decodes to fewer bytes), and is bounded by the
//! route's own byte limit. The structure estimate is [`TOKEN`] for each value
//! and key (each holds a `Value` or `String` in a vector, which grows to up
//! to twice its length, and a string's own allocation), plus [`OBJECT`] for
//! each non-empty object (its first B-tree node) and [`ARRAY`] for each
//! non-empty array (a vector's first allocation holds four values), plus
//! twice the longest string literal with an escape or number: `serde_json`
//! decodes such a string (and, with its `float_roundtrip` feature, which a
//! build may enable, such a number) into a scratch buffer, which it reuses
//! and which grows to up to twice the longest it held.
//!
//! The walk agrees with any JSON parser on where strings begin and end: from
//! the first byte, outside a string a `"` opens one; inside, a `\` escapes
//! the next byte and a `"` closes it. Outside strings every run of bytes that
//! is not whitespace or punctuation counts as one value, which is at least
//! what a parser reads there. A body that is not valid JSON fails its parse
//! at its first invalid byte, so the structure built before that lies in the
//! part both read alike, and the walk counts everything after it too.

/// The structure estimate above which a body is refused before it is parsed,
/// whatever its byte length.
///
/// 16 MiB, eight times the shared 2 MiB body limit, beyond the body's own
/// bytes. Measured (#872) with these constants: 2 MiB of real Claude Code
/// conversation history (32.6 MiB of it, nine sessions over 200 KB)
/// estimates at 3.5 MiB on average and 4.8 MiB for the densest session.
/// Tool definitions are denser: with five properties each, those with an
/// 80-character description per property reach the budget at about 1.5 MiB,
/// those with 20 characters at about 1 MiB, and bare schemas of ten
/// properties at about 0.7 MiB. A refused legitimate request costs more than
/// the memory the budget allows, so it is set above the densest real
/// requests, not the average. Revisit it with the same measurement
/// (`docs/operations.md`, "Request body structure").
pub const BUDGET: usize = 16 * 1024 * 1024;

/// Bytes charged for each value or key (see the module docs).
pub(crate) const TOKEN: usize = 96;
/// Bytes charged for each non-empty object: its first B-tree node.
pub(crate) const OBJECT: usize = 640;
/// Bytes charged for each non-empty array: its first allocation.
pub(crate) const ARRAY: usize = 128;
/// The scratch buffer's smallest allocation, once a string has an escape.
const SCRATCH_MIN: usize = 64;

/// A string literal the walk passed, for a caller that reads them (the image
/// scan): `start..end` is its content between the quotes, and `key` is true
/// when a `:` follows it.
pub(crate) struct Literal {
    pub start: usize,
    pub end: usize,
    pub key: bool,
}

/// The structure estimate of `body`, calling `on_string` with each string
/// literal in order. One pass, no allocation. A literal for which
/// `on_string` returns true is image data the image input buffer accounts
/// for (`inference::image_buffer`, its copies counted there, the parser's
/// scratch copy of it included), so its scratch is not charged here.
pub(crate) fn walk(body: &[u8], mut on_string: impl FnMut(&Literal) -> bool) -> usize {
    let mut tokens = 0usize;
    let mut containers = 0usize;
    let mut longest_scratch = 0usize;
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b']' | b'}' => i += 1,
            b'"' => {
                let start = i + 1;
                let (end, escaped) = string_end(body, start);
                let after = (end + 1).min(body.len());
                let key = next_byte(body, after).is_some_and(|at| body[at] == b':');
                let image = on_string(&Literal { start, end, key });
                if escaped && !image {
                    longest_scratch = longest_scratch.max(end - start);
                }
                tokens += 1;
                i = after;
            }
            open @ (b'{' | b'[') => {
                tokens += 1;
                let close = if open == b'{' { b'}' } else { b']' };
                if next_byte(body, i + 1).is_some_and(|at| body[at] != close) {
                    containers =
                        containers.saturating_add(if open == b'{' { OBJECT } else { ARRAY });
                }
                i += 1;
            }
            _ => {
                // A number, `true`, `false`, `null` or anything else: one
                // value, to the next byte that ends it. A long number may
                // pass through the same scratch buffer (`serde_json` with
                // `float_roundtrip`, which a build may enable), so a run is
                // charged as an escaped string is.
                tokens += 1;
                let run = body[i..]
                    .iter()
                    .position(|b| {
                        matches!(
                            b,
                            b' ' | b'\t'
                                | b'\n'
                                | b'\r'
                                | b','
                                | b':'
                                | b']'
                                | b'}'
                                | b'"'
                                | b'{'
                                | b'['
                        )
                    })
                    .unwrap_or(body.len() - i);
                longest_scratch = longest_scratch.max(run);
                i += run;
            }
        }
    }
    tokens
        .saturating_mul(TOKEN)
        .saturating_add(containers)
        .saturating_add(longest_scratch.saturating_mul(2))
        .saturating_add(if longest_scratch > 0 { SCRATCH_MIN } else { 0 })
}

/// The structure estimate of `body` ([`walk`] without a reader).
pub fn estimate(body: &[u8]) -> usize {
    walk(body, |_| false)
}

/// Whether `body` may be parsed: its structure estimate is within [`BUDGET`].
pub fn within_budget(body: &[u8]) -> bool {
    estimate(body) <= BUDGET
}

/// What the gateway's own plan adds to a batch work line: one object of a
/// few fields (`jobs::lines::LinePlan`), well within this, so a line within
/// [`BUDGET`] at planning is within it with its plan when it runs.
pub const PLAN_ALLOWANCE: usize = 4 * 1024;

/// The index of the `"` that closes the string whose content starts at
/// `start`, or the body's end when it is unterminated, and whether the
/// string holds an escape.
pub(crate) fn string_end(body: &[u8], start: usize) -> (usize, bool) {
    let mut i = start;
    let mut escaped = false;
    loop {
        // Eight bytes at a time while none of them is a quote or a
        // backslash: string content, which only those two bytes can end.
        while let Some(chunk) = body.get(i..i + 8) {
            let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
            if has_byte(word, b'"') || has_byte(word, b'\\') {
                break;
            }
            i += 8;
        }
        match body.get(i) {
            None => return (body.len(), escaped),
            Some(b'\\') => {
                escaped = true;
                i += 2;
            }
            Some(b'"') => return (i, escaped),
            Some(_) => i += 1,
        }
    }
}

/// Whether any byte of `word` is `byte`: exact (the classic zero-byte test
/// on `word` with `byte` cleared, which reports a zero byte if and only if
/// there is one).
const fn has_byte(word: u64, byte: u8) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let x = word ^ (ONES * byte as u64);
    x.wrapping_sub(ONES) & !x & HIGHS != 0
}

/// The index of the first byte at or after `from` that is not whitespace.
fn next_byte(body: &[u8], from: usize) -> Option<usize> {
    body.get(from..)?
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .map(|i| from + i)
}
