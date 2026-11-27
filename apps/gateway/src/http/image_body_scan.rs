//! The bytes of a buffered large body that serde could turn into structure,
//! measured before serde runs (`inference::image_buffer::TEXT_BODY_BYTES`).
//!
//! A body above the shared text limit is admitted on its `Content-Length`
//! for its images. Everything else in it (keys, numbers, arrays, objects and
//! ordinary strings) must fit the text limit, or parsing it could build a
//! tree many times its size. [`outside_images`] walks the JSON string
//! literals with no allocation and sets aside the raw length of at most
//! [`MAX_IMAGES`] of the longest literals that are an image's data: a whole
//! image URL as `image_input::Images::take` accepts it (a PNG, JPEG, WebP or
//! GIF `data:` prefix, then only base64 characters, at most its length
//! bound), or a Messages image source's `data` (the value of a `data` key
//! that is only padded base64, within the same bound); whatever is left is
//! what parsing may turn into structure. Capping the
//! set-aside at the image limit means many small image strings cannot lift
//! the bound, and holding it to the image grammar means what is set aside is
//! printable ASCII: a parser that echoes one in an error (an image URL in a
//! field of another type) writes it once, never escaped into a larger
//! form. The exact check
//! (`image_buffer::LargeBody::allows`, on the parsed images) still runs after
//! the parse.
//!
//! The walk agrees with any JSON parser on where strings begin and end: from
//! the first byte, outside a string a `"` opens one; inside, a `\` escapes
//! the next byte and a `"` closes it. A body that is not valid JSON fails its
//! parse at its first invalid byte, so the structure built before that lies
//! in the part both read alike.
use crate::{
    inference::types::ImageType,
    protocols::image_input::{MAX_BASE64_BYTES, MAX_IMAGES},
};

/// The body's length less the raw lengths of at most [`MAX_IMAGES`] of its
/// longest string literals that are image URLs (Chat Completions, Responses).
pub(super) fn outside_images(body: &[u8]) -> usize {
    scan(body, false)
}

/// [`outside_images`] for a Messages body, whose image data is also the
/// padded base64 value of a `data` key (an image source's).
pub(super) fn outside_messages_images(body: &[u8]) -> usize {
    scan(body, true)
}

fn scan(body: &[u8], data_values: bool) -> usize {
    let mut longest = [0usize; MAX_IMAGES];
    let mut at = 0;
    // Where the string value of the last `data` key opens, if it is one.
    let mut data_value = None;
    while let Some(open) = body[at..].iter().position(|&b| b == b'"') {
        let start = at + open + 1;
        let end = string_end(body, start);
        let literal = &body[start..end];
        // An image is always a value; a key is structure, which parsers copy
        // more often than a value (the error path keeps each key on it).
        let after = (end + 1).min(body.len());
        let colon = body[after..]
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
            .map(|i| after + i)
            .filter(|&i| body[i] == b':');
        if let Some(colon) = colon {
            // A key: remember where its value opens when that value is a
            // string, so only that literal is read as the key's value.
            data_value = (data_values && literal == b"data")
                .then(|| {
                    body[colon + 1..]
                        .iter()
                        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                        .map(|i| colon + 1 + i)
                        .filter(|&i| body[i] == b'"')
                        .map(|i| i + 1)
                })
                .flatten();
            at = after;
            continue;
        }
        let source_data = data_value.take() == Some(start) && is_base64(literal);
        if source_data || is_image_url(literal) {
            // Keep the longest: replace the shortest held so far.
            let (shortest, held) = longest
                .iter_mut()
                .enumerate()
                .min_by_key(|(_, len)| **len)
                .map(|(i, len)| (i, *len))
                .unwrap_or((0, 0));
            if literal.len() > held {
                longest[shortest] = literal.len();
            }
        }
        // Past the closing quote (or the end of an unterminated string).
        at = (end + 1).min(body.len());
    }
    body.len() - longest.iter().sum::<usize>()
}

/// The index of the `"` that closes the string whose content starts at
/// `start`, or the body's end when it is unterminated.
fn string_end(body: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < body.len() {
        match body[i] {
            b'\\' => i += 2,
            b'"' => return i,
            _ => i += 1,
        }
    }
    body.len()
}

/// The raw literal's characters, decoded one byte each, or `None` at an
/// escape no image URL can contain. JSON may escape any character, and some
/// encoders write `/` as `\/`; an image URL is ASCII, so only `\/` and an
/// ASCII `\uXXXX` decode, and anything else ends the walk.
fn decoded(literal: &[u8]) -> impl Iterator<Item = Option<u8>> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let &b = literal.get(i)?;
        if b != b'\\' {
            i += 1;
            return Some(Some(b));
        }
        let (escaped, width) = match literal.get(i + 1) {
            Some(b'/') => (Some(b'/'), 2),
            Some(b'u') => (
                literal
                    .get(i + 2..i + 6)
                    .and_then(hex4)
                    .and_then(|code| u8::try_from(code).ok())
                    .filter(u8::is_ascii),
                6,
            ),
            _ => (None, 0),
        };
        i = if escaped.is_some() {
            i + width
        } else {
            literal.len()
        };
        Some(escaped)
    })
}

/// Whether a raw literal decodes to an image URL `Images::take` could accept:
/// a PNG, JPEG, WebP or GIF data URL prefix, then one to
/// [`MAX_BASE64_BYTES`] characters of the base64 alphabet.
fn is_image_url(literal: &[u8]) -> bool {
    let mut chars = decoded(literal);
    let mut prefix = [0u8; 24];
    let mut len = 0;
    while len < prefix.len() {
        match chars.next() {
            Some(Some(b)) => {
                prefix[len] = b;
                len += 1;
                if b == b',' {
                    break;
                }
            }
            _ => return false,
        }
    }
    if !ImageType::ALL
        .into_iter()
        .any(|t| t.data_url_prefix().as_bytes() == &prefix[..len])
    {
        return false;
    }
    base64_length(chars).is_some()
}

/// Whether a raw literal decodes to padded base64 alone, one to
/// [`MAX_BASE64_BYTES`] characters (a Messages image source's `data`).
fn is_base64(literal: &[u8]) -> bool {
    base64_length(decoded(literal)).is_some_and(|n| n % 4 == 0)
}

/// The count of decoded characters when all are base64, at most
/// [`MAX_BASE64_BYTES`] and at least one.
fn base64_length(chars: impl Iterator<Item = Option<u8>>) -> Option<usize> {
    let mut data = 0usize;
    for c in chars {
        match c {
            Some(b) if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=') => {
                data += 1;
                if data > MAX_BASE64_BYTES {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (data > 0).then_some(data)
}

/// Four hex digits, exactly as JSON writes them (no sign).
fn hex4(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0u32, |code, &d| {
        char::from(d).to_digit(16).map(|v| code * 16 + v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(len: usize) -> String {
        format!("data:image/png;base64,{}", "A".repeat(len))
    }

    #[test]
    fn image_literals_are_set_aside_and_everything_else_counts() {
        let url = image(1000);
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}},{{"type":"image_url","image_url":{{"url":"{url}"}}}}]}}]}}"#
        );
        assert_eq!(outside_images(body.as_bytes()), body.len() - url.len());
        // Structure and ordinary strings count in full.
        let text = r#"{"input":[{"a":0},{"a":0}],"tool_choice":[0,0,0],"s":"data:imagex"}"#;
        assert_eq!(outside_images(text.as_bytes()), text.len());
    }

    #[test]
    fn at_most_max_images_literals_are_set_aside_the_longest_first() {
        let small = image(10);
        let large = image(1000);
        let mut items: Vec<String> = (0..100).map(|_| format!("\"{small}\"")).collect();
        items.extend((0..MAX_IMAGES).map(|_| format!("\"{large}\"")));
        items.extend((0..5).map(|_| format!("\"{small}\"")));
        let body = format!("[{}]", items.join(","));
        // Only the twenty large ones; every small one counts.
        assert_eq!(
            outside_images(body.as_bytes()),
            body.len() - MAX_IMAGES * large.len()
        );
        // Twenty-one equal literals: one of them counts.
        let body = format!(
            "[{}]",
            vec![format!("\"{large}\""); MAX_IMAGES + 1].join(",")
        );
        assert_eq!(
            outside_images(body.as_bytes()),
            body.len() - MAX_IMAGES * large.len()
        );
    }

    #[test]
    fn escapes_decide_string_boundaries_and_what_decodes() {
        // An escaped quote does not end the string: the brackets after it
        // are string content, as a parser reads them, and the image literal
        // after it is found where a parser finds it.
        let url = image(4);
        let body = format!(r#"["AA\"]]]]","{url}"]"#);
        assert_eq!(outside_images(body.as_bytes()), body.len() - url.len());
        // `\/` and `\u002f` decode to `/`, `\u0064` to `d`, `\u0041` to `A`.
        for literal in [
            r"data:image\/png;base64,AA\/A",
            r"data:image\u002fpng;base64,AAAA",
            r"\u0064ata:image/png;base64,AAAA",
            r"data:image/png;base64,AA\u0041A",
        ] {
            let body = format!("[\"{literal}\"]");
            assert_eq!(outside_images(body.as_bytes()), 4, "{literal}");
        }
        // Not an image URL once decoded.
        for literal in [
            r"data:imagE/png;base64,AAAA",
            r"data:image\npng;base64,AAAA",
            r"data:image\u00",
            r"data:image\u+02fpng;base64,AAAA",
            r#"data:image/png;base64,AA\"AA"#,
            r"data:image/png;base64,AAéA",
            r"data:image/png;base64,AA\u00e9A",
        ] {
            let body = format!("[\"{literal}\"]");
            assert_eq!(outside_images(body.as_bytes()), body.len(), "{literal}");
        }
        // An unterminated string and a trailing backslash end the walk, and
        // neither is an image URL, so the whole body counts.
        for body in [r#"["data:image/png"#, "[\"data:image/png;base64,AA\\"] {
            assert_eq!(outside_images(body.as_bytes()), body.len(), "{body}");
        }
    }

    #[test]
    fn an_image_url_as_a_key_counts_in_full() {
        let url = image(1000);
        for body in [
            format!(r#"{{"input":{{"{url}":{{"{url}" :1}}}}}}"#),
            format!("{{\"{url}\"\n\t :1}}"),
        ] {
            assert_eq!(outside_images(body.as_bytes()), body.len(), "{body}");
        }
        // The same string as a value is set aside.
        let body = format!(r#"{{"k":"{url}","x":1}}"#);
        assert_eq!(outside_images(body.as_bytes()), body.len() - url.len());
    }

    #[test]
    fn only_a_whole_image_url_within_its_bound_is_set_aside() {
        let counts_in_full = |literal: &str| {
            let body = format!("[\"{literal}\"]");
            outside_images(body.as_bytes()) == body.len()
        };
        for prefix in ["png", "jpeg", "webp", "gif"] {
            assert!(!counts_in_full(&format!("data:image/{prefix};base64,AAAA")));
        }
        // Another type, no data, a byte outside the base64 alphabet (a parser
        // would escape DEL in an error message at six times its size), or a
        // body longer than any image may be: counted in full.
        assert!(counts_in_full("data:image/bmp;base64,AAAA"));
        assert!(counts_in_full("data:image/png;base64,"));
        assert!(counts_in_full(&format!(
            "data:image/png;base64,{}\u{7f}A",
            "A".repeat(100)
        )));
        assert!(counts_in_full(&format!(
            "data:image/png;base64,{}A",
            "A".repeat(MAX_BASE64_BYTES)
        )));
        assert!(!counts_in_full(&format!(
            "data:image/png;base64,{}",
            "A".repeat(MAX_BASE64_BYTES)
        )));
    }

    #[test]
    fn the_route_cap_is_the_largest_body_the_bound_can_pass() {
        use crate::inference::image_buffer::{MAX_BODY_BYTES, TEXT_BODY_BYTES};
        // Twenty JPEG URLs at their base64 limit, quoted and comma-separated
        // in an array, padded with whitespace to the cap: exactly the text
        // limit is left outside them.
        let url = format!("data:image/jpeg;base64,{}", "A".repeat(MAX_BASE64_BYTES));
        let items = vec![format!("\"{url}\""); MAX_IMAGES].join(",");
        let mut body = format!("[{items}]").into_bytes();
        body.resize(MAX_BODY_BYTES, b' ');
        assert_eq!(outside_images(&body), TEXT_BODY_BYTES);
        body.push(b' ');
        assert_eq!(outside_images(&body), TEXT_BODY_BYTES + 1);
    }

    #[test]
    fn a_data_value_is_image_data_only_in_a_messages_body() {
        let base64 = "A".repeat(4096);
        // A Messages source: its `data` is set aside, whatever whitespace
        // stands around the colon.
        for body in [
            format!(r#"{{"source":{{"type":"base64","data":"{base64}"}}}}"#),
            format!("{{\"data\" :\n \"{base64}\"}}"),
        ] {
            assert_eq!(
                outside_messages_images(body.as_bytes()),
                body.len() - base64.len(),
                "{body}"
            );
            // On Chat and Responses a `data` key is no image (a tool
            // schema's, an audio part's): it counts in full.
            assert_eq!(outside_images(body.as_bytes()), body.len(), "{body}");
        }
        // Only the key's own string value: not a nested one, not a value
        // spelled "data", not base64 that is unpadded or not base64.
        for body in [
            format!(r#"{{"data":{{"x":"{base64}"}}}}"#),
            format!(r#"{{"k":"data","v":"{base64}"}}"#),
            format!(r#"{{"data":"{}"}}"#, "A".repeat(4097)),
            format!(r#"{{"data":"{base64}!"}}"#),
        ] {
            assert_eq!(
                outside_messages_images(body.as_bytes()),
                body.len(),
                "{body}"
            );
        }
    }
}
