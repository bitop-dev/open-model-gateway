use super::*;

#[test]
fn incomplete_prices_cannot_win_by_price_or_evidence() {
    let model = json!({"id":"v/m","pricing":{"prompt":"3","completion":"3"}});
    for missing in ["prompt", "completion", "request"] {
        let mut cheap = json!({"provider_name":"Incomplete","pricing":{"prompt":"0","completion":"0","request":"0"}});
        if missing == "request" {
            // A missing request fee is none (the next test); an unreadable one
            // is as incomplete as a missing token rate.
            cheap["pricing"]["request"] = json!(0);
        } else {
            cheap["pricing"].as_object_mut().unwrap().remove(missing);
        }
        let endpoints = json!({"endpoints":[cheap,{"provider_name":"Complete","pricing":{"prompt":"1","completion":"1","request":"0"}}]});
        for evidence in [
            vec![],
            vec![Evidence {
                input: 0,
                output: 0,
                cost: 0,
            }],
        ] {
            let out = map_with_endpoints(
                &model,
                WorkloadKind::Generation,
                None,
                Some(&endpoints),
                &evidence,
            );
            assert_eq!(out["endpoint"]["provider"], "Complete", "{missing}: {out}");
        }
    }
}

/// The shape OpenRouter serves: endpoint prices carry no `request` key and an
/// absent ceiling is an explicit null. Neither makes the endpoint unusable.
#[test]
fn endpoints_without_a_request_fee_or_with_null_ceilings_are_comparable() {
    let model = json!({"id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"3","completion":"3"}});
    let endpoints = json!({"endpoints":[
        {"provider_name":"Dear","status":0,"max_completion_tokens":null,"pricing":{"prompt":"0.000002","completion":"0.000004"}},
        {"provider_name":"Cheap","status":0,"max_completion_tokens":null,"pricing":{"prompt":"0.000001","completion":"0.000002","input_cache_read":"0.0000001"}}
    ]});
    let evidence = Evidence {
        input: 1_000_000,
        output: 1_000_000,
        cost: 6_000_000,
    };
    for (evidence, provider, selection) in [
        (vec![], "Cheap", "cheapest"),
        (vec![evidence], "Dear", "evidence"),
    ] {
        let out = map_with_endpoints(
            &model,
            WorkloadKind::Generation,
            None,
            Some(&endpoints),
            &evidence,
        );
        assert_eq!(out["endpoint"]["provider"], provider, "{out}");
        assert_eq!(out["endpoint"]["selection"], selection, "{out}");
        let warnings = out["warnings"].to_string();
        assert!(!warnings.contains("token ceiling was unusable"), "{out}");
        assert!(
            !warnings.contains("missing or unusable comparison rates"),
            "{out}"
        );
    }
}

/// A free endpoint can be the cheapest, and is never offered without review.
#[test]
fn a_zero_priced_cheapest_endpoint_needs_review() {
    let model = json!({"id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"3","completion":"3"}});
    let endpoints = json!({"endpoints":[{"provider_name":"Free","status":0,"context_length":4096,"max_completion_tokens":100,"pricing":{"prompt":"0","completion":"0"}}]});
    let out = map_with_endpoints(
        &model,
        WorkloadKind::Generation,
        None,
        Some(&endpoints),
        &[],
    );
    assert_eq!(out["endpoint"]["provider"], "Free", "{out}");
    assert_eq!(out["needs_review"], true, "{out}");
    assert!(
        out["warnings"].to_string().contains("lists a zero price"),
        "{out}"
    );
}

#[test]
fn unreadable_status_is_unknown_and_unusable_ceilings_keep_catalog_limits() {
    let model = json!({"id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"3","completion":"3"}});
    for status in [json!("0"), json!(0.0), Value::Null, json!(false)] {
        let endpoints = json!({"endpoints":[
            {"provider_name":"Unknown","status":status,"pricing":{"prompt":"0","completion":"0","request":"0"}},
            {"provider_name":"Up","status":0,"pricing":{"prompt":"1","completion":"1","request":"0"}}
        ]});
        let out = map_with_endpoints(
            &model,
            WorkloadKind::Generation,
            None,
            Some(&endpoints),
            &[],
        );
        assert_eq!(out["endpoint"]["provider"], "Up", "{status}: {out}");
    }
    for limit in [json!(0), json!(2147483648u64), json!("100"), json!(-1)] {
        let endpoints = json!({"endpoints":[{"provider_name":"Up","context_length":limit,"max_completion_tokens":limit,"pricing":{"prompt":"1","completion":"1","request":"0"}}]});
        let out = map_with_endpoints(
            &model,
            WorkloadKind::Generation,
            None,
            Some(&endpoints),
            &[],
        );
        assert_eq!(out["draft"]["input_token_limit"], 4096, "{limit}");
        assert_eq!(out["draft"]["output_token_limit"], 100, "{limit}");
        assert!(out["warnings"].to_string().contains("unusable"), "{out}");
        assert_eq!(out["needs_review"], true);
    }
}

#[test]
fn provider_labels_remove_format_controls() {
    let label = selection::provider_label(
        &json!({"provider_name":"A\u{202e}B\u{2066}C\u{200b}D\u{feff}\n"}),
    );
    assert_eq!(label, "ABCD");
}

#[test]
fn hostile_entries_are_bounded_and_do_not_offer_partial_prices() {
    for count in [65, 512, 20_000] {
        let overrides: Vec<_> = (1..=count)
            .map(|n| json!({"min_prompt_tokens":n,"prompt":"1"}))
            .collect();
        let model =
            json!({"id":"v/m","pricing":{"prompt":"1","completion":"2","overrides":overrides}});
        let out = map(&model, WorkloadKind::Generation, None);
        assert!(out["lines"].as_array().unwrap().len() <= 64);
        assert!(out["warnings"].as_array().unwrap().len() <= 64);
        assert!(out.to_string().len() < 16_384);
        assert_eq!(out["needs_review"], true);
        assert!(
            out["draft"]["price_lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["unknown"] == true),
            "{out}"
        );
    }
    // Individually bounded tiers can still exceed the total line budget.
    let model = json!({"pricing":{"prompt":"1","completion":"2","overrides":(1..=64).map(|n|json!({"min_prompt_tokens":n,"prompt":"1","completion":"2"})).collect::<Vec<_>>()}});
    let out = map(&model, WorkloadKind::Generation, None);
    assert!(out["lines"].as_array().unwrap().len() <= 64);
    assert!(
        out["draft"]["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unknown"] == true)
    );
    // Neither a missing endpoint list nor an unpriced endpoint turns a refused
    // catalog import into a selected price.
    let incomplete = json!({"endpoints":[{"pricing":{"prompt":"1"}}]});
    for endpoints in [None, Some(&incomplete)] {
        let fallback = map_with_endpoints(&model, WorkloadKind::Generation, None, endpoints, &[]);
        assert_eq!(
            fallback, out,
            "catalog fallback must preserve the entire refusal"
        );
    }
    let endpoints =
        json!({"endpoints":[{"pricing":{"prompt":"1","completion":"2","request":"0"}}]});
    let selected = map_with_endpoints(
        &model,
        WorkloadKind::Generation,
        None,
        Some(&endpoints),
        &[],
    );
    assert!(
        selected["draft"]["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unknown"] == true)
    );
    assert!(
        selected.get("endpoint").is_none(),
        "a refusal has no priced endpoint"
    );
    assert!(!selected["warnings"].to_string().contains("Priced from"));
    let body = json!({"endpoints":[{"pricing":(0..20_000).map(|n|json!({"billable":"output_image","unit":"image","cost_usd":"1","variant":n.to_string()})).collect::<Vec<_>>()}]}).to_string();
    let out = map(
        &json!({"id":"v/m"}),
        WorkloadKind::Images,
        Some(body.as_bytes()),
    );
    assert!(out.to_string().len() < 16_384);
    assert!(
        out["draft"]["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unknown"] == true)
    );
}

#[test]
fn refused_imports_keep_the_models_token_ceilings() {
    let model = json!({"id":"v/limited", "context_length":4096,
        "top_provider":{"max_completion_tokens":100}, "pricing":{"prompt":"1"}});
    let too_many = vec![Value::Null; limits::MAX_ITEMS + 1];
    let mut oversized_model = model.clone();
    oversized_model["pricing"]["overrides"] = json!(too_many);
    let oversized_endpoints = json!({"endpoints":too_many});
    for (entry, endpoints) in [
        (&oversized_model, None),
        (&model, Some(&oversized_endpoints)),
    ] {
        let out = map_with_endpoints(entry, WorkloadKind::Generation, None, endpoints, &[]);
        assert_eq!(out["draft"]["input_token_limit"], 4096);
        assert_eq!(out["draft"]["output_token_limit"], 100);
        assert_eq!(
            out["ceilings"],
            json!({"context_length":4096,"max_completion_tokens":100,
            "input_token_limit":4096,"output_token_limit":100})
        );
        assert_eq!(out["catalog_model_id"], "v/limited");
        assert_eq!(out["needs_review"], true);
        assert!(out.get("endpoint").is_none());
        assert!(
            out["draft"]["price_lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["unknown"] == true)
        );
    }
}

#[test]
fn cache_tier_with_omitted_or_null_base_requires_manual_base_rate() {
    for cache_read in [None, Some(Value::Null)] {
        let mut catalog = json!({"pricing": {
            "prompt": "0",
            "completion": "0",
            "overrides": [{"min_prompt_tokens": 100, "input_cache_read": "0.000001"}]
        }});
        if let Some(value) = cache_read {
            catalog["pricing"]["input_cache_read"] = value;
        }
        let out = map(&catalog, WorkloadKind::Generation, None);
        let draft = out["draft"]["price_lines"].as_array().unwrap();
        let cache: Vec<_> = draft
            .iter()
            .filter(|row| row["meter"] == "cache_read_tokens")
            .collect();
        assert_eq!(
            cache,
            vec![&json!({"meter":"cache_read_tokens","unknown":true})]
        );
        assert!(out["lines"].as_array().unwrap().iter().any(|row| {
            row["meter"] == "cache_read_tokens"
                && row["min_prompt_tokens"] == 100
                && row["microusd_per_batch"].is_string()
        }));
    }
}

#[test]
fn distinct_variants_across_bounded_endpoints_still_refuse_at_the_line_budget() {
    let endpoints: Vec<_> = (0..2).map(|endpoint| json!({"pricing": (0..64).map(|n| json!({"billable":"output_image","unit":"image","cost_usd":"1","variant":format!("size-{}",endpoint*64+n)})).collect::<Vec<_>>()})).collect();
    let body = json!({"endpoints":endpoints}).to_string();
    let out = map(
        &json!({"id":"v/m"}),
        WorkloadKind::Images,
        Some(body.as_bytes()),
    );
    assert!(out.to_string().len() < 16_384);
    assert_eq!(out["needs_review"], true);
    assert!(
        out["draft"]["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unknown"] == true)
    );
}

#[test]
fn a_refused_endpoint_retains_catalog_ceilings() {
    let tiers: Vec<_> = (1..=64)
        .map(|n| json!({"min_prompt_tokens":n,"prompt":"2"}))
        .collect();
    let model = json!({
        "id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},
        "pricing":{"prompt":"1","completion":"1","overrides":tiers}
    });
    let endpoints = json!({"endpoints":[{
        "provider_name":"Example","status":0,"context_length":8192,"max_completion_tokens":1024,
        "pricing":{"prompt":"1","completion":"1"}
    }]});
    let out = map_with_endpoints(
        &model,
        WorkloadKind::Generation,
        None,
        Some(&endpoints),
        &[],
    );
    assert!(out.get("endpoint").is_none());
    assert!(
        out["draft"]["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unknown"] == true)
    );
    assert_eq!(out["draft"]["input_token_limit"], 4096);
    assert_eq!(out["draft"]["output_token_limit"], 100);
}

#[test]
fn a_non_object_model_cannot_panic_when_an_endpoint_is_selected() {
    let endpoints =
        json!({"endpoints":[{"pricing":{"prompt":"1","completion":"1","request":"0"}}]});
    for model in [json!(true), json!([]), json!("x"), json!(42)] {
        let out = map_with_endpoints(
            &model,
            WorkloadKind::Generation,
            None,
            Some(&endpoints),
            &[],
        );
        assert_eq!(out["needs_review"], true);
    }
}

#[tokio::test]
async fn a_malformed_success_is_not_cached() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let h = h.clone();
            async move {
                let body = match h.fetch_add(1, Ordering::SeqCst) {
                    0 => "{broken",
                    1 => "{\"data\":{}}",
                    2 => "[[]]",
                    _ => "{\"data\":[]}",
                };
                ([("content-type", "application/json")], body)
            }
        }),
    );
    let (url, task) = tests::mock(app).await;
    let catalog = OpenRouterCatalog::for_test(url, Duration::from_secs(2));
    for _ in 0..3 {
        assert!(catalog.get("/models").await.is_err());
    }
    assert!(catalog.get("/models").await.is_ok());
    assert!(catalog.get("/models").await.is_ok());
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    task.abort();
}

proptest::proptest! {
    #![proptest_config(crate::proptest_support_tests::config(100))]
    #[test]
    fn oversized_tiers_and_endpoints_never_publish_a_prefix(count in 65_usize..2000) {
        let list: Vec<_> = (1..=count).map(|n| json!({"min_prompt_tokens":n,"prompt":"1"})).collect();
        let model = json!({"id":"v/m","pricing":{"prompt":"1","completion":"1","overrides":list}});
        let out = map(&model, WorkloadKind::Generation, None);
        proptest::prop_assert!(out.to_string().len() < 16_384);
        proptest::prop_assert_eq!(&out["needs_review"], &json!(true));
        proptest::prop_assert!(out["draft"]["price_lines"].as_array().unwrap().iter().all(|p|p["unknown"] == true));
        let endpoints = json!({"endpoints": vec![json!({"pricing":{"prompt":"0","completion":"0","request":"0"}});count]});
        let out = map_with_endpoints(&json!({"id":"v/m"}), WorkloadKind::Generation, None, Some(&endpoints), &[]);
        proptest::prop_assert!(out.to_string().len() < 16_384);
        proptest::prop_assert_eq!(&out["needs_review"], &json!(true));
        proptest::prop_assert!(out["draft"]["price_lines"].as_array().unwrap().iter().all(|p|p["unknown"] == true));
    }
}

#[test]
fn decoded_complexity_and_warnings_are_bounded_before_mapping() {
    let sparse = format!("[{}]", "null,".repeat(140_000).trim_end_matches(','));
    assert!(sparse.len() < BODY_CAP);
    assert!(limits::catalog_json(sparse.as_bytes()).is_err());
    for body in [b"{} trailing".as_slice(), b"{broken", b"[1e9999]"] {
        assert!(limits::catalog_json(body).is_err());
    }
    assert!(limits::catalog_json(br#"{"data":[{"pricing":{"prompt":"1e400"}}]}"#).is_ok());
    let mut warnings = Warnings::default();
    for n in 0..1000 {
        warnings.push(format!("Warning {n}"));
        warnings.push(format!("Warning {n}"));
    }
    assert_eq!(warnings.items.len(), 64);
    assert!(warnings.items.last().unwrap().contains("omitted"));
    let overrides: Vec<_> = (1..=64)
        .map(|n| {
            let mut o = json!({"min_prompt_tokens":n});
            o[format!("extra_{n}")] = json!("1");
            o
        })
        .collect();
    let full = json!({"context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"0","completion":"0","request":"0","input_cache_read":"0","input_cache_write":"0","input_cache_write_1h":"0","overrides":overrides}});
    let out = map(&full, WorkloadKind::Generation, None);
    assert!(out["display_lines"].is_array(), "otherwise fully priced");
    assert_eq!(out["warnings"].as_array().unwrap().len(), 64);
    assert_eq!(out["needs_review"], true, "omitted warnings require review");
    let model = json!({"pricing":{"overrides":vec![json!({});64]}});
    let out = map(&model, WorkloadKind::Generation, None);
    assert_eq!(
        out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w
                .as_str()
                .unwrap()
                .contains("without a prompt-size threshold"))
            .count(),
        1
    );
}
