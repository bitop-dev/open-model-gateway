//! The native Messages envelope (`MessagesOptions::native_messages`): the
//! validated messages as Anthropic receives them, each image's data a
//! placeholder the body sent splices (`providers::image_body`).
use super::*;

/// The native envelope of the messages. An image block's base64 is not
/// copied: its `data` is the placeholder of that image (numbered in wire
/// order, as the parser takes images), and the body sent splices the
/// canonical image's base64 in (`image_body::json_body_base64`), so an
/// image's bytes are held once.
pub(super) fn native_messages(wire: &[WireMessage]) -> Vec<Value> {
    let mut images = 0;
    wire.iter()
        .map(|m| {
            let mut content = match &m.content {
                Value::String(text) => json!([{"type":"text","text":text}]),
                Value::Array(blocks) => Value::Array(
                    blocks
                        .iter()
                        .map(|block| native_block(block, &mut images))
                        .collect(),
                ),
                other => other.clone(),
            };
            if let Some(blocks) = content.as_array_mut() {
                for block in blocks {
                    if block["type"] == "tool_result" && block.get("content").is_none() {
                        block["content"] = json!("");
                    }
                }
            }
            json!({"role":m.role,"content":content})
        })
        .collect()
}
/// One content block of the native envelope, at the two places the parser
/// takes images: a message's content and a tool result's content.
fn native_block(block: &Value, images: &mut usize) -> Value {
    match block["type"].as_str() {
        Some("image") => without_data(block, images),
        // Built field by field, so the content's images are never cloned.
        Some("tool_result") => match (block.as_object(), block.get("content")) {
            (Some(fields), Some(Value::Array(inner))) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let value = if key == "content" {
                            Value::Array(
                                inner
                                    .iter()
                                    .map(|b| {
                                        if b["type"] == "image" {
                                            without_data(b, images)
                                        } else {
                                            b.clone()
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            value.clone()
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            _ => block.clone(),
        },
        _ => block.clone(),
    }
}
/// An image block with its source's `data` replaced by the next placeholder.
fn without_data(block: &Value, images: &mut usize) -> Value {
    let Some(fields) = block.as_object() else {
        return block.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in fields {
        let value = match (key.as_str(), value.as_object()) {
            ("source", Some(source)) => Value::Object(
                source
                    .iter()
                    .map(|(k, v)| {
                        let v = if k == "data" {
                            json!(crate::providers::image_body::placeholder(*images))
                        } else {
                            v.clone()
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            ),
            _ => value.clone(),
        };
        out.insert(key.clone(), value);
    }
    *images += 1;
    Value::Object(out)
}
