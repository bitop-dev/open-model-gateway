//! The bytes of a buffered large body that serde could turn into structure,
//! measured before serde runs (`inference::image_buffer::TEXT_BODY_BYTES`).
//!
//! A body above the shared text limit is admitted on its `Content-Length`
//! for its images. Everything else in it (keys, numbers, arrays, objects and
//! ordinary strings) must fit the text limit, or parsing it could build a
//! tree many times its size. [`outside_images`] walks the JSON string
//! literals with no allocation and sets aside the raw length of at most
//! [`MAX_IMAGES`] of the longest literals whose text begins with
//! `data:image/`; whatever is left is what parsing may turn into structure.
//! Capping the set-aside at the image limit means many small `data:image/`
//! strings cannot lift the bound. The exact check
//! (`image_buffer::LargeBody::allows`, on the parsed images) still runs after
//! the parse.
//!
//! The walk agrees with any JSON parser on where strings begin and end: from
//! the first byte, outside a string a `"` opens one; inside, a `\` escapes
//! the next byte and a `"` closes it. A body that is not valid JSON fails its
//! parse at its first invalid byte, so the structure built before that lies
//! in the part both read alike.
use crate::protocols::image_input::MAX_IMAGES;

const PREFIX: &[u8] = b"data:image/";

/// The body's length less the raw lengths of at most [`MAX_IMAGES`] of its
/// longest `data:image/` string literals.
pub(super) fn outside_images(body: &[u8]) -> usize {
    let mut longest = [0usize; MAX_IMAGES];
    let mut at = 0;
    while let Some(open) = body[at..].iter().position(|&b| b == b'"') {
        let start = at + open + 1;
        let end = string_end(body, start);
        let literal = &body[start..end];
        if begins_with_image_prefix(literal) {
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

/// Whether a raw literal's decoded text begins with `data:image/`. JSON may
/// escape any character, and some encoders escape `/` as `\/`, so the first
/// characters are decoded, not compared byte for byte.
fn begins_with_image_prefix(literal: &[u8]) -> bool {
    let mut i = 0;
    for &want in PREFIX {
        let Some(&b) = literal.get(i) else {
            return false;
        };
        let (got, width) = if b == b'\\' {
            match literal.get(i + 1) {
                Some(b'u') => match literal.get(i + 2..i + 6).and_then(hex4) {
                    Some(code) if code < 0x80 => (code as u8, 6),
                    _ => return false,
                },
                Some(&escaped) => (escaped, 2),
                None => return false,
            }
        } else {
            (b, 1)
        };
        if got != want {
            return false;
        }
        i += width;
    }
    true
}

fn hex4(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|d| u32::from_str_radix(d, 16).ok())
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
    fn escapes_decide_string_boundaries_and_the_prefix() {
        // An escaped quote does not end the string, so the brackets after it
        // are string content, as a parser reads them.
        let body = r#"["data:image/png;base64,AA\"]]]]","x"]"#;
        let literal = r#"data:image/png;base64,AA\"]]]]"#;
        assert_eq!(outside_images(body.as_bytes()), body.len() - literal.len());
        // `\/` and `/` decode to `/`.
        for literal in [
            r"data:image\/png;base64,AAAA",
            r"data:image/png;base64,AAAA",
            r"data:image/png;base64,AAAA",
        ] {
            let body = format!("[\"{literal}\"]");
            assert_eq!(outside_images(body.as_bytes()), 4, "{literal}");
        }
        // Not the prefix once decoded.
        for literal in [r"data:imagE/png", r"data:image\npng", r"data:image\u00"] {
            let body = format!("[\"{literal}\"]");
            assert_eq!(outside_images(body.as_bytes()), body.len(), "{literal}");
        }
        // An unterminated string and a trailing backslash end the walk.
        for body in [r#"["data:image/png"#, "[\"data:image/png\\"] {
            assert!(outside_images(body.as_bytes()) <= body.len());
        }
    }
}
