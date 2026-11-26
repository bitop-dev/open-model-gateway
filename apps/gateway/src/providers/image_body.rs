//! Upstream JSON bodies that carry inline images (#24), serialized without
//! copying an image into an intermediate JSON value.
//!
//! An encoder writes [`placeholder`]`(k)` where the `k`-th image of the
//! request (in message order) belongs; [`json_body`] serializes the value
//! and splices each image's data URL into its placeholder, so every image is
//! copied once, into the body that is sent. A placeholder holds a nonce
//! drawn once per process from the OS generator, so client text cannot
//! forge one; each must occur exactly once in the serialized value, or the
//! request is refused before anything is sent.
use std::sync::LazyLock;

use serde_json::Value;

use serde_json::json;

use crate::inference::{
    error::InferenceError,
    types::{ChatRequest, Content, ImageInput, Part},
};

static NONCE: LazyLock<String> =
    LazyLock::new(|| format!("omg-image-{}", uuid::Uuid::new_v4().simple()));

/// The stand-in for the `index`-th image of the request being encoded.
pub(crate) fn placeholder(index: usize) -> String {
    format!("{}-{index}", *NONCE)
}

/// The request's images, in the order encoders number them.
pub(crate) fn images(request: &ChatRequest) -> impl Iterator<Item = &ImageInput> {
    request
        .messages
        .iter()
        .filter_map(|m| m.content.as_ref())
        .flat_map(Content::images)
}

/// Chat Completions content: a string, or parts whose images are
/// placeholders the body splices (`image_body`), numbered in message order.
pub(crate) fn chat_content(content: Option<&Content>, images: &mut usize) -> Value {
    match content {
        None => Value::Null,
        Some(Content::Text(text)) => json!(text),
        Some(Content::Parts(parts)) => Value::Array(
            parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => json!({"type":"text","text":text}),
                    Part::Image(image) => {
                        let mut url = json!({"url": placeholder(*images)});
                        *images += 1;
                        if let Some(detail) = image.detail() {
                            url["detail"] = json!(detail.as_str());
                        }
                        json!({"type":"image_url","image_url":url})
                    }
                })
                .collect(),
        ),
    }
}

/// `value` as JSON bytes, with the request's images spliced in (data URLs).
pub(crate) fn json_body(value: &Value, request: &ChatRequest) -> Result<Vec<u8>, InferenceError> {
    let json = serde_json::to_vec(value).map_err(|_| InferenceError::InvalidRequest)?;
    let images: Vec<&ImageInput> = images(request).collect();
    let nonce = NONCE.as_bytes();
    if occurrences(&json, nonce) != images.len() {
        return Err(InferenceError::InvalidField("image"));
    }
    if images.is_empty() {
        return Ok(json);
    }
    let extra: usize = images.iter().map(|i| i.data_url().len()).sum();
    let mut out = Vec::with_capacity(json.len() + extra);
    // Encoders number placeholders in message order and serialization keeps
    // that order, so each marker is found after the previous one; a body in
    // another order fails the search and is refused, never misspliced.
    let mut rest = &json[..];
    for (index, image) in images.iter().enumerate() {
        let marker = format!("\"{}\"", placeholder(index));
        let at = find(rest, marker.as_bytes()).ok_or(InferenceError::InvalidField("image"))?;
        // Keep both quotes; the data URL needs no JSON escaping (base64
        // and the fixed `data:<type>;base64,` prefix only).
        out.extend_from_slice(&rest[..=at]);
        out.extend_from_slice(image.data_url().as_bytes());
        rest = &rest[at + marker.len() - 1..];
    }
    out.extend_from_slice(rest);
    Ok(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut rest = haystack;
    while let Some(at) = find(rest, needle) {
        count += 1;
        rest = &rest[at + needle.len()..];
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::types::{ImageType, Message, Part, Role};
    use base64::Engine as _;

    fn image(bytes: &[u8]) -> ImageInput {
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        ImageInput::new(url, ImageType::Png, None, bytes.len())
    }

    fn request(parts: Vec<Part>) -> ChatRequest {
        let messages = if parts.is_empty() {
            vec![]
        } else {
            vec![Message {
                role: Role::User,
                content: Some(Content::Parts(parts)),
                tool_calls: vec![],
                tool_call_id: None,
                tool_result_is_error: false,
            }]
        };
        ChatRequest {
            messages_options: None,
            model: "m".into(),
            messages,
            tools: vec![],
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            stream: false,
        }
    }

    #[test]
    fn images_are_spliced_in_order_and_text_is_untouched() {
        let png = b"\x89PNG\r\n\x1a\n1";
        let gif = b"\x89PNG\r\n\x1a\n2";
        let r = request(vec![
            Part::Text("look \"here\"".into()),
            Part::Image(image(png)),
            Part::Image(image(gif)),
        ]);
        let value = serde_json::json!({"content":[
            {"type":"text","text":"look \"here\""},
            {"url": placeholder(0)},
            {"url": placeholder(1)},
        ]});
        let body = json_body(&value, &r).unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        let urls: Vec<&ImageInput> = images(&r).collect();
        assert_eq!(parsed["content"][0]["text"], "look \"here\"");
        assert_eq!(parsed["content"][1]["url"], urls[0].data_url());
        assert_eq!(parsed["content"][2]["url"], urls[1].data_url());
        assert!(!String::from_utf8_lossy(&body).contains(NONCE.as_str()));
    }

    #[test]
    fn a_missing_or_repeated_placeholder_refuses_the_body() {
        let r = request(vec![Part::Image(image(b"\x89PNG\r\n\x1a\n"))]);
        for value in [
            serde_json::json!({"url": "nothing"}),
            serde_json::json!({"a": placeholder(0), "b": placeholder(0)}),
            serde_json::json!({"a": placeholder(0), "text": format!("x {}", placeholder(0))}),
        ] {
            assert_eq!(
                json_body(&value, &r).err(),
                Some(InferenceError::InvalidField("image")),
                "{value}"
            );
        }
        // Text-only requests serialize exactly as before.
        let text = request(vec![]);
        let value = serde_json::json!({"model":"m","messages":[]});
        assert_eq!(
            json_body(&value, &text).unwrap(),
            serde_json::to_vec(&value).unwrap()
        );
    }
}
