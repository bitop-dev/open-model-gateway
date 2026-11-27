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
//! allocations), the error message of a body it refuses included. A body
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
//! and which grows to up to twice the longest it held; plus three times the
//! longest string as an error message would show it (a refused body's
//! message holds one of its strings, escaped for display, and grows by
//! doubling), which for printable text is its own length.
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
/// estimates at about 3.5 MiB on average and 4.9 MiB for the densest session (its strings part 0.2 MiB: its longest string is 32 KiB).
/// Tool definitions are denser: with five properties each, those with an
/// 80-character description per property reach the budget at about 1.5 MiB,
/// those with 20 characters at about 1 MiB, and bare schemas of ten
/// properties at about 0.7 MiB. A refused legitimate request costs more than
/// the memory the budget allows, so it is set above the densest real
/// requests, not the average. Text an error message shows escaped is
/// charged at that length (combining marks, as in fully vocalized Arabic,
/// about eight times), so one such string over about 1.9 MiB is refused.
/// Revisit it with the same measurement
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
/// An error message's text besides the one string it may hold (what it
/// expected: a type, or the names of a type's fields).
const MESSAGE_MIN: usize = 4096;

/// A body's estimate in two parts: its `structure` (values, keys, objects
/// and arrays), and its `strings`: the parser's scratch copy of its longest
/// string with an escape or number and a refused body's error message,
/// which hold one string each and so are at most about five times the
/// body's own length, whatever its structure.
#[derive(Clone, Copy)]
pub struct Weight {
    pub structure: usize,
    pub strings: usize,
}
impl Weight {
    /// Both parts: what the parse may allocate beyond the body's bytes.
    pub fn total(self) -> usize {
        self.structure.saturating_add(self.strings)
    }
}

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
pub(crate) fn walk(body: &[u8], mut on_string: impl FnMut(&Literal) -> bool) -> Weight {
    let mut tokens = 0usize;
    let mut containers = 0usize;
    let mut longest_scratch = 0usize;
    let mut longest_shown = 0usize;
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b']' | b'}' => i += 1,
            b'"' => {
                let start = i + 1;
                let StringEnd {
                    end,
                    escaped,
                    plain,
                } = string_end(body, start);
                let after = (end + 1).min(body.len());
                let key = next_byte(body, after).is_some_and(|at| body[at] == b':');
                let image = on_string(&Literal { start, end, key });
                if !image {
                    if escaped {
                        longest_scratch = longest_scratch.max(end - start);
                    }
                    let shown = if plain {
                        end - start
                    } else {
                        shown_len(&body[start..end])
                    };
                    longest_shown = longest_shown.max(shown);
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
    Weight {
        structure: tokens.saturating_mul(TOKEN).saturating_add(containers),
        strings: longest_scratch
            .saturating_mul(2)
            .saturating_add(if longest_scratch > 0 { SCRATCH_MIN } else { 0 })
            // A refused body's error message, which holds one of its
            // strings shown for display, in a message that grows by
            // doubling (each step holds the old and the new buffer: up to
            // three times its length).
            .saturating_add(longest_shown.saturating_mul(3))
            .saturating_add(MESSAGE_MIN),
    }
}

/// The most bytes a string literal's decoded text takes when an error
/// message shows it (`{:?}`, which escapes every character `escape_debug`
/// escapes; `{}` shows no more), for a literal that is not plain printable
/// ASCII (which shows as it is, [`StringEnd::plain`]): decoded and measured
/// character by character. A literal that is not valid JSON text is
/// measured as raw bytes, each shown at most as `\u{ff}`.
fn shown_len(literal: &[u8]) -> usize {
    let mut shown = 0usize;
    let mut i = 0;
    while i < literal.len() {
        let (c, width) = match literal[i] {
            b'\\' => match literal.get(i + 1) {
                Some(b'u') => match escaped_char(literal, i) {
                    Some(decoded) => decoded,
                    None => return literal.len().saturating_mul(6),
                },
                Some(&e) => (
                    match e {
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        other => char::from(other),
                    },
                    2,
                ),
                None => return literal.len().saturating_mul(6),
            },
            _ => {
                let rest = &literal[i..];
                let width = utf8_width(rest[0]).min(rest.len());
                match std::str::from_utf8(&rest[..width])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    Some(c) => (c, width),
                    None => return literal.len().saturating_mul(6),
                }
            }
        };
        shown += c.escape_debug().map(char::len_utf8).sum::<usize>();
        i += width;
    }
    shown
}

/// The character a `\u` escape at `at` decodes to (a surrogate pair taken
/// together), and the escape's width.
fn escaped_char(literal: &[u8], at: usize) -> Option<(char, usize)> {
    let hex = |from: usize| -> Option<u32> {
        let digits = literal.get(from..from + 4)?;
        u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    };
    let high = hex(at + 2)?;
    if (0xd800..0xdc00).contains(&high) && literal.get(at + 6..at + 8) == Some(b"\\u") {
        let low = hex(at + 8)?;
        let code = 0x10000 + ((high - 0xd800) << 10) + (low.checked_sub(0xdc00)?);
        return Some((char::from_u32(code)?, 12));
    }
    // A lone surrogate decodes to U+FFFD at most (`serde_json` refuses it).
    Some((char::from_u32(high).unwrap_or('\u{fffd}'), 6))
}

/// The byte length of the UTF-8 character that `lead` starts.
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xf0..=0xf7 => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// The estimate of `body` ([`walk`] without a reader), both parts.
pub fn estimate(body: &[u8]) -> usize {
    weight(body).total()
}

/// The two parts of `body`'s estimate.
pub fn weight(body: &[u8]) -> Weight {
    walk(body, |_| false)
}

/// Whether `body` may be parsed: its estimate is within [`BUDGET`].
pub fn within_budget(body: &[u8]) -> bool {
    estimate(body) <= BUDGET
}

/// Whether a batch line may be parsed: twice its structure within
/// [`BUDGET`] and `allowance`. A line is parsed into a tree and then
/// converted into its request (`jobs::lines`), the tree still held while the
/// request is built: two structures at once. Its strings part is not
/// compared: it is at most about five times the line, which
/// `BATCH_MAX_LINE_BYTES` bounds (4 MiB), and lines are parsed one at a
/// time, so a line holding one long escaped string (an image written with
/// `\/`) is not refused for it.
pub fn line_within_budget(line: &[u8], allowance: usize) -> bool {
    weight(line).structure.saturating_mul(2) <= BUDGET.saturating_add(allowance)
}

/// What the gateway's own plan adds to a batch work line: one object of a
/// few fields (`jobs::lines::LinePlan`), well within this, so a line within
/// [`BUDGET`] at planning is within it with its plan when it runs.
pub const PLAN_ALLOWANCE: usize = 4 * 1024;

/// The end of a string literal: the index of the `"` that closes it (the
/// body's end when it is unterminated), whether it holds an escape, and
/// whether every byte of it is printable ASCII (`0x20..0x7f`) with no
/// escape, which an error message shows as it is.
pub(crate) struct StringEnd {
    pub end: usize,
    pub escaped: bool,
    pub plain: bool,
}

/// [`StringEnd`] of the string whose content starts at `start`.
pub(crate) fn string_end(body: &[u8], start: usize) -> StringEnd {
    let mut i = start;
    let mut escaped = false;
    let mut special = false;
    loop {
        // Eight bytes at a time while none of them is a quote or a
        // backslash: string content, which only those two bytes can end.
        while let Some(chunk) = body.get(i..i + 8) {
            let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
            if has_byte(word, b'"') || has_byte(word, b'\\') {
                break;
            }
            special |= !printable_ascii(word);
            i += 8;
        }
        match body.get(i) {
            None => {
                return StringEnd {
                    end: body.len(),
                    escaped,
                    plain: !escaped && !special,
                };
            }
            Some(b'\\') => {
                escaped = true;
                i += 2;
            }
            Some(b'"') => {
                return StringEnd {
                    end: i,
                    escaped,
                    plain: !escaped && !special,
                };
            }
            Some(&b) => {
                special |= !(0x20..0x7f).contains(&b);
                i += 1;
            }
        }
    }
}

/// Whether every byte of `word` is printable ASCII (`0x20..0x7f`): none has
/// its high bit, none is below `0x20` (the classic less-than test, exact for
/// bytes without the high bit) and none is DEL.
const fn printable_ascii(word: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    word & HIGHS == 0
        && word.wrapping_sub(ONES * 0x20) & !word & HIGHS == 0
        && !has_byte(word, 0x7f)
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
