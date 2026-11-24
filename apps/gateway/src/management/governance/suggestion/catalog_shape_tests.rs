//! The mapper against documents shaped like the ones OpenRouter serves:
//! model entries carry many fields the mapper never reads (speech models list
//! up to a hundred voices), endpoint price objects usually have no `request`
//! key and absent ceilings are explicit nulls. The fixtures are hand-written
//! in that shape; none of them is copied from the live catalog.
use super::*;

/// A speech model entry in the catalog's shape with `voices` voices.
fn speech_model(voices: usize) -> Value {
    json!({
        "id": "vendor/tts-1", "canonical_slug": "vendor/tts-1-20260716",
        "hugging_face_id": null, "name": "Vendor: TTS 1", "created": 1_784_237_167,
        "description": "A multilingual text-to-speech model.",
        "context_length": 0,
        "architecture": {"modality": "text->speech", "input_modalities": ["text"],
            "output_modalities": ["speech"], "tokenizer": "Other", "instruct_type": null},
        "pricing": {"prompt": "0.00003", "completion": "0"},
        "top_provider": {"context_length": 0, "max_completion_tokens": 0, "is_moderated": false},
        "per_request_limits": null, "supported_parameters": [],
        "default_parameters": {"temperature": null, "top_p": null, "top_k": null},
        "knowledge_cutoff": null, "expiration_date": null,
        "links": {"details": "/api/v1/models/vendor/tts-1-20260716/endpoints"},
        "endpoints": ["/api/v1/audio/speech"],
        "supported_voices": (0..voices).map(|n| format!("voice-{n}-en")).collect::<Vec<_>>(),
    })
}

/// A chat model entry in the catalog's shape.
fn chat_model() -> Value {
    json!({
        "id": "vendor/chat-1", "canonical_slug": "vendor/chat-1", "name": "Vendor: Chat 1",
        "context_length": 131_072,
        "architecture": {"modality": "text->text", "input_modalities": ["text"], "output_modalities": ["text"]},
        "pricing": {"prompt": "0.00000002", "completion": "0.00000005"},
        "top_provider": {"context_length": 131_072, "max_completion_tokens": 16_384, "is_moderated": false},
        "supported_parameters": ["max_tokens", "temperature", "top_p", "stop", "seed"],
        "endpoints": ["/api/v1/chat/completions", "/api/v1/responses"],
    })
}

/// One endpoint in the shape `/models/{id}/endpoints` serves: no `request`
/// key, a numeric discount, null ceilings and many fields the mapper ignores.
fn endpoint(provider: &str, prompt: &str, completion: &str) -> Value {
    json!({
        "name": format!("{provider} | vendor/chat-1"), "model_id": "vendor/chat-1",
        "model_name": "Vendor: Chat 1", "context_length": 131_072,
        "pricing": {"prompt": prompt, "completion": completion, "discount": 0},
        "provider_name": provider, "tag": provider.to_ascii_lowercase(), "service_tier": null,
        "quantization": "fp8", "max_completion_tokens": null, "max_prompt_tokens": null,
        "supported_parameters": ["max_tokens", "temperature", "top_p", "stop", "frequency_penalty",
            "presence_penalty", "repetition_penalty", "top_k", "seed", "min_p", "logit_bias",
            "response_format", "tools", "tool_choice", "structured_outputs"],
        "supports_tool_choice": {"none": true, "auto": true, "required": true, "function": true},
        "status": 0, "uptime_last_30m": 100, "uptime_last_5m": 100, "uptime_last_1d": 99.5,
        "supports_implicit_caching": false, "native_tools": {}, "supports_voice_cloning": false,
        "latency_last_30m": null, "throughput_last_30m": null,
    })
}

fn endpoints(list: Vec<Value>) -> Value {
    json!({"data": {"id": "vendor/chat-1", "name": "Vendor: Chat 1", "endpoints": list}})
}

fn all_unknown(out: &Value) -> bool {
    out["draft"]["price_lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|line| line["unknown"] == true)
}

fn rate<'a>(out: &'a Value, meter: &str) -> &'a Value {
    &out["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["meter"] == meter && line["min_prompt_tokens"].is_null())
        .unwrap()["microusd_per_batch"]
}

/// Values the mapper reads from a model: `id`, `context_length`,
/// `top_provider.max_completion_tokens` and the whole `pricing` tree.
fn read_values(model: &Value) -> usize {
    fn count(v: &Value) -> usize {
        1 + match v {
            Value::Array(a) => a.iter().map(count).sum(),
            Value::Object(o) => o.values().map(count).sum(),
            _ => 0,
        }
    }
    ["id", "context_length", "pricing"]
        .iter()
        .filter_map(|k| model.get(*k))
        .map(count)
        .sum::<usize>()
        + model
            .get("top_provider")
            .and_then(|t| t.get("max_completion_tokens"))
            .map_or(0, count)
}

#[test]
fn speech_models_with_long_voice_lists_are_priced() {
    // The live catalog lists 66, 90 and 97 voices on speech models.
    for voices in [64, 65, 66, 90, 97, 1_000] {
        let model = speech_model(voices);
        for out in [
            map(&model, WorkloadKind::AudioSpeech, None),
            map_with_endpoints(&model, WorkloadKind::AudioSpeech, None, None, &[]),
        ] {
            assert!(!all_unknown(&out), "{voices} voices: {out}");
            // $0.00003 per character is $30 per million characters.
            assert_eq!(rate(&out, "input_characters"), "30000000", "{voices}");
            assert_eq!(rate(&out, "output_audio_seconds_ms"), "0", "{voices}");
        }
    }
}

#[test]
fn unread_fields_never_refuse_and_read_fields_keep_their_limits() {
    let mut model = chat_model();
    model["supported_parameters"] = json!((0..500).map(|n| n.to_string()).collect::<Vec<_>>());
    model["architecture"]["extra"] = json!(
        (0..100)
            .map(|n| (n.to_string(), json!(n)))
            .collect::<serde_json::Map<_, _>>()
    );
    model["description"] = json!("d".repeat(20_000));
    let out = map(&model, WorkloadKind::Generation, None);
    assert!(!all_unknown(&out), "{out}");
    assert_eq!(rate(&out, "input_tokens"), "20000");
    // The output echoes no field the mapper does not read.
    assert!(!out.to_string().contains("supported_parameters"));

    let mut wide = chat_model();
    for n in 0..62 {
        wide["pricing"][format!("extra_{n}")] = json!("1");
    }
    assert_eq!(wide["pricing"].as_object().unwrap().len(), 64);
    assert!(!all_unknown(&map(&wide, WorkloadKind::Generation, None)));
    let mut wide = chat_model();
    for n in 0..63 {
        wide["pricing"][format!("extra_{n}")] = json!("1");
    }
    assert_eq!(wide["pricing"].as_object().unwrap().len(), 65);
    assert!(all_unknown(&map(&wide, WorkloadKind::Generation, None)));
    let mut long = chat_model();
    long["id"] = json!("v".repeat(16_385));
    assert!(all_unknown(&map(&long, WorkloadKind::Generation, None)));
    let mut long = chat_model();
    long["id"] = json!("v".repeat(16_384));
    assert!(!all_unknown(&map(&long, WorkloadKind::Generation, None)));
}

/// 4,096 values in the fields the mapper reads are accepted; one more refuses.
#[test]
fn the_mapping_value_budget_is_exact() {
    let base = json!({"id":"v/m","pricing":{"prompt":"1","completion":"1"}});
    let fixed = read_values(&base); // id, pricing, prompt, completion
    for (total, priced) in [(4_096, true), (4_097, false)] {
        let mut model = base.clone();
        let mut rest = total - fixed;
        let mut groups = vec![];
        rest -= 1; // the `padding` array
        while rest > 0 {
            let nulls = (rest - 1).min(64);
            groups.push(Value::Array(vec![Value::Null; nulls]));
            rest -= nulls + 1;
        }
        assert!(groups.len() <= 64);
        model["pricing"]["padding"] = Value::Array(groups);
        assert_eq!(read_values(&model), total);
        let out = map(&model, WorkloadKind::Generation, None);
        assert_eq!(!all_unknown(&out), priced, "{total} values");
    }
}

#[test]
fn long_live_shaped_endpoint_lists_are_compared() {
    // One live model lists 51 endpoints; each carries about 45 values.
    for count in [51, 90, 256] {
        let list: Vec<Value> = (0..count)
            .map(|n| {
                endpoint(
                    &format!("Provider{n}"),
                    &format!("0.000000{:02}", 99 - n % 90),
                    "0.00000005",
                )
            })
            .collect();
        let out = map_with_endpoints(
            &chat_model(),
            WorkloadKind::Generation,
            None,
            Some(&endpoints(list)),
            &[],
        );
        assert_eq!(out["endpoint"]["selection"], "cheapest", "{count}: {out}");
        assert_eq!(out["endpoint"]["endpoint_count"], count);
        assert!(!all_unknown(&out));
        assert!(
            !out["warnings"]
                .to_string()
                .contains("token ceiling was unusable"),
            "null ceilings are absent: {out}"
        );
    }
    let list = vec![endpoint("P", "0.00000002", "0.00000005"); 257];
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&endpoints(list)),
        &[],
    );
    assert!(all_unknown(&out), "more than 256 endpoints refuse");
    assert!(out.get("endpoint").is_none());
    // A read endpoint field over its limits still refuses the whole draft.
    let mut odd = endpoint("P", "0.00000002", "0.00000005");
    odd["provider_name"] = json!("p".repeat(16_385));
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&endpoints(vec![odd])),
        &[],
    );
    assert!(all_unknown(&out));
}

/// An endpoint whose rates are complete but whose status cannot be read is
/// left out of the cheapest choice, and the draft says so.
#[test]
fn an_excluded_unreadable_status_is_reported() {
    let mut hidden = endpoint("Hidden", "0.00000001", "0.00000001");
    hidden["status"] = json!("0");
    let list = endpoints(vec![hidden, endpoint("Up", "0.00000002", "0.00000005")]);
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&list),
        &[],
    );
    assert_eq!(out["endpoint"]["provider"], "Up");
    assert_eq!(out["needs_review"], true);
    let warnings = out["warnings"].to_string();
    assert!(warnings.contains("unreadable status"), "{warnings}");
    // A down endpoint (an integer status) is not current and needs no note.
    let mut down = endpoint("Down", "0.00000001", "0.00000001");
    down["status"] = json!(-5);
    let list = endpoints(vec![down, endpoint("Up", "0.00000002", "0.00000005")]);
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&list),
        &[],
    );
    assert!(!out["warnings"].to_string().contains("unreadable status"));
}

/// Endpoints are compared on one combined figure; the warnings state both
/// token rates so a lower input rate is never shown as the higher price.
#[test]
fn price_warnings_show_the_rates_that_were_compared() {
    // Dear: $1/M input, $10/M output (11). Cheap: $2/M input, $2/M output (4).
    let list = endpoints(vec![
        endpoint("Dear", "0.000001", "0.00001"),
        endpoint("Cheap", "0.000002", "0.000002"),
    ]);
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&list),
        &[],
    );
    assert_eq!(out["endpoint"]["provider"], "Cheap");
    let warnings: Vec<&str> = out["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    let priced = warnings
        .iter()
        .find(|w| w.starts_with("Priced from"))
        .unwrap();
    assert!(
        priced.contains("$2/M input tokens and $2/M output tokens"),
        "{priced}"
    );
    assert!(
        priced.contains("one million input tokens, one million output tokens and one request"),
        "{priced}"
    );
    let pricier = warnings
        .iter()
        .find(|w| w.contains("may route to another endpoint"))
        .unwrap();
    assert!(
        pricier.contains("$1/M input tokens and $10/M output tokens on Dear"),
        "{pricier}"
    );
}

/// Speech and transcription endpoints are not priced per token; the
/// comparison text names the workload's units, never tokens it does not have.
#[test]
fn endpoint_rates_are_stated_in_the_workload_units() {
    let speech = speech_model(3);
    let list = endpoints(vec![
        endpoint("Voice", "0.00003", "0"),
        endpoint("Dearer", "0.00004", "0"),
    ]);
    let out = map_with_endpoints(&speech, WorkloadKind::AudioSpeech, None, Some(&list), &[]);
    assert_eq!(out["endpoint"]["input_rate"], "$30/M input characters");
    let text = out["warnings"].to_string();
    for needle in [
        "Priced from the Voice endpoint ($30/M input characters and $0/M output units)",
        "for one million input characters, one million output units and one request",
        "($40/M input characters and $0/M output units on Dearer)",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert!(!text.contains("token"), "{text}");
    let stt = json!({"id": "vendor/stt-1", "pricing": {"prompt": "0.00000333", "completion": "0"}});
    let list = endpoints(vec![endpoint("Ear", "0.00000333", "0")]);
    let out = map_with_endpoints(
        &stt,
        WorkloadKind::AudioTranscriptions,
        None,
        Some(&list),
        &[],
    );
    assert_eq!(out["endpoint"]["input_rate"], "$3.33/M input units");
    let text = out["warnings"].to_string();
    assert!(
        text.contains("($3.33/M input units and $0/M output tokens)"),
        "{text}"
    );
}

/// When every endpoint with comparable rates is left out for an unreadable
/// status, the catalog fallback says that status was the reason.
#[test]
fn a_fallback_for_unreadable_statuses_says_so() {
    let mut hidden = endpoint("Hidden", "0.00000001", "0.00000001");
    hidden["status"] = json!("0");
    let incomplete = json!({"provider_name": "Incomplete", "pricing": {"prompt": "0"}});
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&endpoints(vec![hidden, incomplete.clone()])),
        &[],
    );
    assert_eq!(out["endpoint"]["selection"], "catalog");
    assert_eq!(out["needs_review"], true);
    let text = out["warnings"].to_string();
    assert!(text.contains("prices were unavailable"), "{text}");
    assert!(text.contains("unreadable status"), "{text}");
    // Without an unreadable status the generic fallback stands alone.
    let mut down = endpoint("Down", "0.00000001", "0.00000001");
    down["status"] = json!(-1);
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&endpoints(vec![down, incomplete])),
        &[],
    );
    assert_eq!(out["endpoint"]["selection"], "catalog");
    assert!(!out["warnings"].to_string().contains("unreadable status"));
}

/// Hostile endpoint pricing can fill the mapper's warnings; the endpoint the
/// draft was priced from is still named.
#[test]
fn selection_warnings_survive_a_full_warning_list() {
    // As many selection warnings as one draft can carry: a free cheapest
    // endpoint with an unusable ceiling, a pricier one, an incomplete one, an
    // unreadable status and a model tier that is not carried over.
    let mut cheap = endpoint("Cheap", "0", "0");
    cheap["max_completion_tokens"] = json!(-1);
    cheap["pricing"]["overrides"] = json!(
        (1..=64)
            .map(|n| json!({"min_prompt_tokens": n, format!("extra_{n}"): "1"}))
            .collect::<Vec<_>>()
    );
    let mut hidden = endpoint("Hidden", "0.000001", "0.000001");
    hidden["status"] = json!(null);
    let list = endpoints(vec![
        cheap,
        endpoint("Dear", "0.000003", "0.000003"),
        json!({"provider_name": "Incomplete", "pricing": {"prompt": "0"}}),
        hidden,
    ]);
    // The model's prompt-size tier does not carry over to other rates.
    let mut model = chat_model();
    model["pricing"]["overrides"] = json!([{"min_prompt_tokens": 1000, "prompt": "0.00000004"}]);
    let out = map_with_endpoints(&model, WorkloadKind::Generation, None, Some(&list), &[]);
    assert_eq!(out["endpoint"]["provider"], "Cheap", "{out}");
    let warnings = out["warnings"].as_array().unwrap();
    assert!(warnings.len() <= 64);
    let text = out["warnings"].to_string();
    let selection = [
        "token ceiling was unusable",
        "Prompt-size price tiers",
        "missing or unusable comparison rates",
        "unreadable status",
        "Priced from the Cheap endpoint",
        "lists a zero price",
        "may route to another endpoint",
    ];
    for needle in selection.iter().chain(&["were omitted"]) {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert_eq!(warnings.len(), limits::MAPPER_WARNINGS + selection.len());
    assert!(selection.len() <= limits::SELECTION_WARNINGS);
    assert_eq!(out["needs_review"], true);
}

#[test]
fn provider_labels_are_plain_visible_text() {
    let provider_label = |e: &Value| selection::sanitized_label(e).0;
    for (endpoint, label) in [
        (
            json!({"provider_name": "Cloud\u{2028}flare\u{2029}"}),
            "Cloud flare",
        ),
        (
            json!({"provider_name": "Amazon\u{a0}\u{3000}Bedrock"}),
            "Amazon Bedrock",
        ),
        (
            json!({"provider_name": "  Deep\u{3164}Infra\t"}),
            "DeepInfra",
        ),
        (
            json!({"provider_name": "A\u{202e}B\u{2066}C\u{200b}D\u{feff}\n"}),
            "ABCD",
        ),
        // Look-alike letters from other scripts are not shown as Latin ones.
        (json!({"provider_name": "\u{421}loudflare"}), "loudflare"),
        // An empty or invisible name falls back to the tag.
        (json!({"provider_name": "", "tag": "groq"}), "groq"),
        (
            json!({"provider_name": "\u{3164}\u{200b} ", "tag": "groq"}),
            "groq",
        ),
        (json!({"provider_name": "", "tag": ""}), "unnamed provider"),
        (json!({"provider_name": 7}), "unnamed provider"),
    ] {
        assert_eq!(provider_label(&endpoint), label, "{endpoint}");
    }
    assert_eq!(
        provider_label(&json!({"provider_name": "x".repeat(500)})).len(),
        64
    );
}

/// A label that sanitising changed, or that two endpoints share, names the
/// endpoint's position, so the catalog cannot borrow a trusted name.
#[test]
fn altered_or_repeated_provider_labels_name_the_endpoint() {
    use selection::provider_label;
    let list = [
        json!({"provider_name": "Cl\u{2603}oudflare"}),
        json!({"provider_name": "Groq"}),
        json!({"provider_name": "Cloudflare"}),
        json!({"provider_name": "\u{200b}", "tag": "lepton"}),
        json!({"provider_name": "Twin"}),
        json!({"tag": "Twin"}),
        json!({"provider_name": "", "tag": "deepinfra"}),
    ];
    let labels: Vec<String> = (0..list.len()).map(|i| provider_label(&list, i)).collect();
    assert_eq!(
        labels,
        [
            "Cloudflare (endpoint 1 of 7)",
            "Groq",
            "Cloudflare (endpoint 3 of 7)",
            "lepton (endpoint 4 of 7)",
            "Twin (endpoint 5 of 7)",
            "Twin (endpoint 6 of 7)",
            "deepinfra",
        ]
    );
    // Through the mapper: the filtered name is the chosen endpoint.
    let mut snow = endpoint("Cloudflare", "0.00000001", "0.00000001");
    snow["provider_name"] = json!("Cl\u{2603}oudflare");
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&endpoints(vec![
            snow,
            endpoint("Dear", "0.000001", "0.000001"),
        ])),
        &[],
    );
    assert_eq!(out["endpoint"]["provider"], "Cloudflare (endpoint 1 of 2)");
    assert!(
        out["warnings"]
            .to_string()
            .contains("Priced from the Cloudflare (endpoint 1 of 2) endpoint"),
        "{out}"
    );
}

#[tokio::test]
async fn concurrent_misses_fetch_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                ([("content-type", "application/json")], "{\"data\":[]}")
            }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let catalog = Arc::new(OpenRouterCatalog::for_test(url, Duration::from_secs(2)));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let catalog = catalog.clone();
            tokio::spawn(async move { catalog.get("/models").await.is_ok() })
        })
        .collect();
    for reader in readers {
        assert!(reader.await.unwrap());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn the_cache_holds_a_bounded_number_of_bytes() {
    // Valid catalog documents of about 3 MiB each, under every decoding limit.
    let body = Arc::new(json!({"data": vec!["x".repeat(16_000); 190]}).to_string());
    assert!(body.len() > 3_000_000 && body.len() < BODY_CAP);
    let served = body.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let served = served.clone();
            async move { ([("content-type", "application/json")], (*served).clone()) }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let catalog = OpenRouterCatalog::for_test(url, Duration::from_secs(5));
    for n in 0..12 {
        assert!(catalog.get(&format!("/models?page={n}")).await.is_ok());
        let held: usize = catalog
            .cache
            .lock()
            .await
            .values()
            .map(|(_, b)| b.len())
            .sum();
        assert!(held <= 16 * 1024 * 1024, "{n}: {held} bytes cached");
    }
    // The newest document is always kept.
    assert!(
        catalog
            .cache
            .lock()
            .await
            .keys()
            .any(|k| k.ends_with("page=11"))
    );
    task.abort();
}

#[tokio::test]
async fn a_body_of_exactly_the_cap_is_read_and_one_byte_more_is_refused() {
    let exact = {
        let mut b = b"{\"data\":[]}".to_vec();
        b.resize(BODY_CAP, b' ');
        b
    };
    let mut over = exact.clone();
    over.push(b' ');
    let app = axum::Router::new()
        .route(
            "/models",
            axum::routing::get(move || {
                let exact = exact.clone();
                async move { ([("content-type", "application/json")], exact) }
            }),
        )
        .route(
            "/models/over",
            axum::routing::get(move || {
                let over = over.clone();
                async move { ([("content-type", "application/json")], over) }
            }),
        );
    let (url, task) = tests::mock(app).await;
    let catalog = OpenRouterCatalog::for_test(url, Duration::from_secs(5));
    assert_eq!(catalog.get("/models").await.unwrap().len(), BODY_CAP);
    assert!(matches!(
        catalog.get("/models/over").await,
        Err(CatalogError::Limit)
    ));
    task.abort();
}

#[test]
fn catalog_decoding_limits_are_exact() {
    // 131,072 values: the array and 131,071 elements.
    let at = format!("[{}]", vec!["0"; 131_071].join(","));
    let over = format!("[{}]", vec!["0"; 131_072].join(","));
    assert!(limits::catalog_json(at.as_bytes()).is_ok());
    assert!(matches!(
        limits::catalog_json(over.as_bytes()),
        Err(CatalogError::Limit)
    ));
    for (len, ok) in [(16_384, true), (16_385, false)] {
        let text = json!(["t".repeat(len)]).to_string();
        let key = json!({"k".repeat(len): 0}).to_string();
        // An escaped string is measured after unescaping.
        let escaped = format!("[\"\\u0041{}\"]", "t".repeat(len - 1));
        for body in [text, key, escaped] {
            assert_eq!(limits::catalog_json(body.as_bytes()).is_ok(), ok, "{len}");
        }
    }
    // Nesting: the outermost value is at depth 0 and depth 96 is the last
    // accepted, so 97 nested arrays are read and 98 are refused as a limit.
    for (levels, ok) in [(97, true), (98, false)] {
        let body = format!("{}{}", "[".repeat(levels), "]".repeat(levels));
        let read = limits::catalog_json(body.as_bytes());
        assert_eq!(read.is_ok(), ok, "{levels}");
        assert!(ok || matches!(read, Err(CatalogError::Limit)), "{levels}");
    }
    let mut exact = b"[]".to_vec();
    exact.resize(BODY_CAP, b' ');
    assert!(limits::catalog_json(&exact).is_ok());
    exact.push(b' ');
    assert!(matches!(
        limits::catalog_json(&exact),
        Err(CatalogError::Limit)
    ));
}

/// An endpoint with every field a full OpenRouter endpoint carries, its
/// price object as large as any sampled: 16 read values.
fn full_endpoint(n: usize) -> Value {
    let mut e = endpoint(&format!("Provider{n}"), "0.00000002", "0.00000005");
    e["pricing"] = json!({
        "prompt": format!("0.000000{:02}", 10 + n % 80), "completion": "0.00000005",
        "request": "0", "image": "0", "web_search": "0", "internal_reasoning": "0.00000005",
        "input_cache_read": "0.00000001", "input_cache_write": "0.00000003", "discount": 0,
    });
    e["max_completion_tokens"] = json!(16_384);
    e
}

/// Values the endpoint projection reads from one endpoint, itself included.
fn endpoint_read_values(e: &Value) -> usize {
    fn count(v: &Value) -> usize {
        1 + match v {
            Value::Array(a) => a.iter().map(count).sum(),
            Value::Object(o) => o.values().map(count).sum(),
            _ => 0,
        }
    }
    1 + [
        "pricing",
        "status",
        "provider_name",
        "tag",
        "context_length",
        "max_completion_tokens",
    ]
    .iter()
    .filter_map(|k| e.get(*k))
    .map(count)
    .sum::<usize>()
}

/// The value tree and the fetched bytes are projected alike.
fn projected(doc: &Value) -> Result<limits::EndpointList, ()> {
    let tree = limits::endpoint_list(doc).map_err(|_| ());
    let bytes = limits::endpoint_list_json(doc.to_string().as_bytes()).map_err(|_| ());
    assert_eq!(tree, bytes, "{doc}");
    tree
}

#[test]
fn full_sized_endpoint_lists_reach_the_endpoint_limit() {
    assert_eq!(endpoint_read_values(&full_endpoint(0)), 16);
    let list = |count: usize| endpoints((0..count).map(full_endpoint).collect());
    let at = list(256);
    assert_eq!(projected(&at).unwrap().unwrap().len(), 256);
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&at),
        &[],
    );
    assert_eq!(out["endpoint"]["selection"], "cheapest", "{out}");
    assert_eq!(out["endpoint"]["endpoint_count"], 256);
    let over = list(257);
    assert!(projected(&over).is_err());
    let out = map_with_endpoints(
        &chat_model(),
        WorkloadKind::Generation,
        None,
        Some(&over),
        &[],
    );
    assert!(all_unknown(&out));
    assert!(out.get("endpoint").is_none());
}

/// Each endpoint has its own budget of read values: 256 are accepted, and
/// one more refuses the draft. A list holds at most 256 × 256.
#[test]
fn the_per_endpoint_value_budget_is_exact() {
    let base =
        json!({"provider_name": "P", "pricing": {"prompt": "0.000001", "completion": "0.000001"}});
    let fixed = endpoint_read_values(&base);
    for (total, priced) in [
        (limits::ENDPOINT_VALUES, true),
        (limits::ENDPOINT_VALUES + 1, false),
    ] {
        let mut e = base.clone();
        let mut rest = total - fixed - 1; // the `padding` array
        let mut groups = vec![];
        while rest > 0 {
            let nulls = (rest - 1).min(64);
            groups.push(Value::Array(vec![Value::Null; nulls]));
            rest -= nulls + 1;
        }
        e["pricing"]["padding"] = Value::Array(groups);
        assert_eq!(endpoint_read_values(&e), total);
        // A list of such endpoints: the budget does not run across them.
        let doc = endpoints(vec![e; 4]);
        assert_eq!(projected(&doc).is_ok(), priced, "{total}");
        let out = map_with_endpoints(
            &chat_model(),
            WorkloadKind::Generation,
            None,
            Some(&doc),
            &[],
        );
        assert_eq!(!all_unknown(&out), priced, "{total} values");
    }
}

/// The read fields of one endpoint keep the mapping limits.
#[test]
fn one_endpoint_s_read_fields_keep_their_limits() {
    let refuses = |e: Value| {
        let doc = endpoints(vec![e, full_endpoint(1)]);
        let refused = projected(&doc).is_err();
        let out = map_with_endpoints(
            &chat_model(),
            WorkloadKind::Generation,
            None,
            Some(&doc),
            &[],
        );
        assert_eq!(all_unknown(&out), refused);
        refused
    };
    for (entries, refused) in [(64, false), (65, true)] {
        let mut e = full_endpoint(0);
        for n in e["pricing"].as_object().unwrap().len()..entries {
            e["pricing"][format!("extra_{n}")] = json!("1");
        }
        assert_eq!(e["pricing"].as_object().unwrap().len(), entries);
        assert_eq!(refuses(e), refused, "{entries} pricing entries");
    }
    for field in ["status", "provider_name", "tag"] {
        for (len, refused) in [(16_384, false), (16_385, true)] {
            let mut e = full_endpoint(0);
            e[field] = json!("s".repeat(len));
            assert_eq!(refuses(e), refused, "{field} of {len} bytes");
        }
    }
}

/// The projection reads the endpoints list as it streams: an over-long list
/// is refused at its 257th entry, before the rest of the body is read, so no
/// tree of the document is ever built. The tail here is not even JSON, which
/// a decoder building the whole tree would report instead.
#[test]
fn endpoint_lists_are_projected_from_the_stream() {
    let mut body = br#"{"data":{"endpoints":["#.to_vec();
    body.extend(std::iter::repeat_n(&b"{},"[..], 257).flatten());
    body.extend_from_slice(b"not json");
    assert!(serde_json::from_slice::<Value>(&body).is_err());
    assert!(matches!(
        limits::endpoint_list_json(&body),
        Err(CatalogError::Limit)
    ));
    // Unread fields are skipped, whatever they hold.
    let tiny = vec![json!({}); 65_000];
    let doc =
        json!({"data": {"other": tiny.clone(), "endpoints": [full_endpoint(0)]}, "more": tiny});
    assert_eq!(projected(&doc).unwrap().unwrap().len(), 1);
    // A repeated key replaces the earlier value, as in a decoded tree.
    let repeated = br#"{"endpoints":[{"provider_name":"A","pricing":{}}],"endpoints":"x"}"#;
    assert_eq!(limits::endpoint_list_json(repeated).unwrap(), None);
    let doc = json!({"data": {"endpoints": []}, "endpoints": [full_endpoint(0)]});
    assert_eq!(projected(&doc).unwrap(), None, "an empty data list wins");
}

/// About 65,000 tiny endpoint objects pass the decoding preflight; they are
/// refused before any tree of them exists, and the refusal is not cached.
#[tokio::test]
async fn a_list_of_many_tiny_endpoints_is_refused_and_not_cached() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let body = json!({"data": {"endpoints": vec![json!({}); 65_000]}}).to_string();
    assert!(limits::catalog_json(body.as_bytes()).is_ok());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/models/v/m/endpoints",
        axum::routing::get(move || {
            let (h, body) = (h.clone(), body.clone());
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                ([("content-type", "application/json")], body)
            }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let catalog = OpenRouterCatalog::for_test(url, Duration::from_secs(5));
    for _ in 0..2 {
        assert!(matches!(
            catalog.endpoints("v/m").await,
            Err(CatalogError::Limit)
        ));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(catalog.cache.lock().await.is_empty());
    task.abort();
}

/// A catalog that accepts the connection and never answers: previews that
/// queue behind the stalled fetch fail within the same deadline, and share
/// its failure instead of each repeating it. A later preview fetches again.
#[tokio::test]
async fn previews_queued_behind_a_stalled_catalog_fail_together() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(60)).await;
                ([("content-type", "application/json")], "{\"data\":[]}")
            }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let timeout = Duration::from_secs(1);
    let catalog = Arc::new(OpenRouterCatalog::for_test(url, timeout));
    let started = Instant::now();
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let catalog = catalog.clone();
            tokio::spawn(async move { catalog.get("/models").await.is_err() })
        })
        .collect();
    for reader in readers {
        assert!(reader.await.unwrap());
    }
    // One after another they would take eight timeouts.
    assert!(started.elapsed() < timeout * 3, "{:?}", started.elapsed());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(catalog.get("/models").await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 2, "a failure is not cached");
    task.abort();
}

/// A preview cancelled while it fetches leaves no outcome behind: the
/// previews waiting for it fetch for themselves and complete.
#[tokio::test]
async fn waiters_complete_when_the_fetching_preview_is_cancelled() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let h = h.clone();
            async move {
                if h.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                ([("content-type", "application/json")], "{\"data\":[]}")
            }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let catalog = Arc::new(OpenRouterCatalog::for_test(url, Duration::from_secs(10)));
    let leader = {
        let catalog = catalog.clone();
        tokio::spawn(async move { catalog.get("/models").await.is_ok() })
    };
    for _ in 0..500 {
        if hits.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the leader is fetching");
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let catalog = catalog.clone();
            tokio::spawn(async move { catalog.get("/models").await.is_ok() })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    leader.abort();
    assert!(leader.await.unwrap_err().is_cancelled());
    for waiter in waiters {
        assert!(waiter.await.unwrap());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    task.abort();
}
