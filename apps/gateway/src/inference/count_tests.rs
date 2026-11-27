use super::*;
use crate::inference::types::{FunctionTool, Message, Role, ToolCall};

fn text(role: Role, content: &str) -> Message {
    Message {
        role,
        content: Some(content.into()),
        tool_calls: vec![],
        tool_call_id: None,
        tool_result_is_error: false,
    }
}
fn request(messages: Vec<Message>, tools: Vec<FunctionTool>) -> ChatRequest {
    ChatRequest {
        messages_options: None,
        model: "m".into(),
        messages,
        tools,
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        stream: false,
    }
}

#[test]
fn the_estimate_is_input_bytes_divided_by_four_rounded_up() {
    assert_eq!(estimate(&request(vec![text(Role::User, "")], vec![])), 0);
    assert_eq!(estimate(&request(vec![text(Role::User, "a")], vec![])), 1);
    assert_eq!(
        estimate(&request(vec![text(Role::User, "abcd")], vec![])),
        1
    );
    assert_eq!(
        estimate(&request(vec![text(Role::User, "abcde")], vec![])),
        2
    );
    // UTF-8 bytes, not characters: three bytes each.
    assert_eq!(
        estimate(&request(vec![text(Role::User, "東京")], vec![])),
        2
    );
    let mut call = text(Role::Assistant, "ok");
    call.tool_calls = vec![ToolCall {
        id: "id".into(),
        name: "fn".into(),
        arguments: "{}".into(),
    }];
    let mut result = text(Role::Tool, "done");
    result.tool_call_id = Some("id".into());
    let tool = FunctionTool {
        name: "fn".into(),
        description: Some("desc".into()),
        parameters: serde_json::json!({"type":"object"}),
        strict: None,
    };
    // system 3 + "ok" 2 + call 6 + result 4 + id 2 + tool 2 + 4 + 17 = 40 bytes.
    let full = request(vec![text(Role::System, "sys"), call, result], vec![tool]);
    assert_eq!(estimate(&full), 10);
}

#[test]
fn an_estimate_is_never_given_for_a_request_with_images() {
    let text = request(vec![text(Role::User, "abcd")], vec![]);
    let count = estimated(&text).unwrap();
    assert_eq!(
        (count.input_tokens, count.source),
        (1, TokenCountSource::Estimate)
    );
    // An inline image: the estimate would leave it out.
    let inline = crate::providers::contract::image_request_with(false, None);
    assert_eq!(
        estimated(&inline).err(),
        Some(InferenceError::InvalidField("image"))
    );
    // A Messages image block is a canonical image, so it is refused the
    // same way (`protocols::messages` parses it into one).
}

#[test]
fn the_limiter_admits_a_fixed_number_per_key_and_minute() {
    let limiter = Limiter::default();
    let (key, other) = (Uuid::new_v4(), Uuid::new_v4());
    for _ in 0..COUNTS_PER_KEY_PER_MINUTE {
        assert!(limiter.admit(key, 7, COUNTS_PER_KEY_PER_MINUTE));
    }
    assert!(
        !limiter.admit(key, 7, COUNTS_PER_KEY_PER_MINUTE),
        "the key's minute is used up"
    );
    assert!(
        limiter.admit(other, 7, COUNTS_PER_KEY_PER_MINUTE),
        "other keys have their own window"
    );
    assert!(
        limiter.admit(key, 8, COUNTS_PER_KEY_PER_MINUTE),
        "the next minute starts again"
    );
}

#[test]
fn the_limiter_table_is_bounded() {
    let limiter = Limiter::default();
    for _ in 0..TRACKED_KEYS {
        assert!(limiter.admit(Uuid::new_v4(), 1, 1));
    }
    // Full of current-minute keys: a new key is refused, never stored.
    assert!(!limiter.admit(Uuid::new_v4(), 1, 1));
    // A later minute drops the old windows first.
    assert!(limiter.admit(Uuid::new_v4(), 2, 1));
    assert_eq!(limiter.windows.lock().unwrap().len(), 1);
}

#[test]
fn a_key_holds_at_most_its_in_flight_slots_and_dropping_frees_them() {
    let in_flight = InFlight::default();
    let (key, other) = (Uuid::new_v4(), Uuid::new_v4());
    let first = in_flight.enter(key, 2).expect("first slot");
    let second = in_flight.enter(key, 2).expect("second slot");
    assert!(
        in_flight.enter(key, 2).is_none(),
        "the key's slots are full"
    );
    let other_slot = in_flight
        .enter(other, 2)
        .expect("other keys have their own");
    drop(first);
    let third = in_flight
        .enter(key, 2)
        .expect("a dropped slot is free again");
    drop((second, third, other_slot));
    assert!(
        in_flight.0.lock().unwrap().is_empty(),
        "a key with nothing running has no entry"
    );
}

#[test]
fn the_source_header_says_how_the_count_was_made() {
    assert_eq!(TokenCountSource::Provider.header(), "provider");
    assert_eq!(
        TokenCountSource::Estimate.header(),
        "estimate; method=utf8-bytes-div-4"
    );
}
