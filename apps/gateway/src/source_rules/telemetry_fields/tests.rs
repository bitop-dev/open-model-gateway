use super::*;

#[test]
fn additions_and_source_substitutions_change_the_inventory() {
    for (path, source, from, to) in [
        (
            "ops_log.rs",
            "enum OpsEvent { Demo { kind: Kind } }",
            "kind: Kind",
            "kind: Kind, mode: Mode",
        ),
        (
            "worker.rs",
            "fn run() { ops_log::emit(OpsEvent::Demo { kind: first }); }",
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"impl OpsEvent { fn attributes(&self) -> Labels { vec![("kind", self.first())] } }"#,
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"fn log_record() { record["traceId"] = json!(first); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first)",
            "first), (\"mode\", second)",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { let labels = [("kind", first)]; family.get_or_create(&labels); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn create() { series(&family, "kind", |i| first[i]); }"#,
            "first",
            "second",
        ),
        (
            "worker.rs",
            r#"fn run() { select! { _ = tick() => METRICS.observe_cache("one two", "ok") } }"#,
            "one two",
            "onetwo",
        ),
    ] {
        let before = inventory(&parse_fixture(path, source));
        let after = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(before, after, "{path}: {from} -> {to}");
    }
}

#[test]
fn test_only_producers_are_excluded() {
    let source = parse_fixture(
        "metrics.rs",
        r#"#[cfg(test)] fn count() { family.get_or_create(&[("kind", "demo")]); }"#,
    );
    assert!(inventory(&source).is_empty());
}

#[test]
fn producer_source_fixtures() {
    let cases = [
        (
            "worker.rs",
            "fn f(){ select! { () = tick() => { emit(event_one()); } } }",
            "event_one",
            "event_two",
        ),
        (
            "worker.rs",
            r#"fn f(){ select! { () = tick() => timer.phase("one") } }"#,
            "one",
            "two",
        ),
        (
            "metrics/collection.rs",
            r#"impl Metrics { fn f(&self){ self.collection_error("one"); } }"#,
            "one",
            "two",
        ),
        (
            "otlp.rs",
            r#"fixed_enum!(Delivery { Accepted => "one" });"#,
            "one",
            "two",
        ),
        (
            "telemetry.rs",
            r#"fixed_enum!(DropReason { Contexts => "contexts" });"#,
            "contexts",
            "context_limit",
        ),
        (
            "metrics_export.rs",
            r#"fixed_enum!(DropReason { Label => "one" });"#,
            "one",
            "two",
        ),
    ];

    for (path, source, from, to) in cases {
        let baseline = inventory(&parse_fixture(path, source));
        let changed = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(baseline, changed, "{path}: {source}");
        let unrelated = format!("fn unrelated() {{ ordinary({to:?}); }} {source}");
        assert_eq!(baseline, inventory(&parse_fixture(path, &unrelated)));
    }
}

#[test]
fn family_uses_resolve_bindings_at_the_lookup() {
    let lookup = "f.get_or_create(&provider);";
    let bound = "let provider = bounded(provider);";
    let check = |body: String| {
        let source = format!(
            "impl Metrics {{ fn set_batch_route_lines(&self) {{ for provider in rows {{ {body} }} }} }}"
        );
        let mut entries = inventory(&parse_fixture("metrics/jobs.rs", &source));
        entries.sort();
        entries
    };
    let macro_lookup = "select! { () = tick() => f.get_or_create(&provider) }";
    assert_ne!(
        check(format!("{bound} {macro_lookup}")),
        check(format!("{macro_lookup} {bound}"))
    );
    let bounded = check(format!("{bound} {lookup}"));
    assert_ne!(bounded, check(format!("{lookup} {bound}")));
    assert_ne!(bounded, check(format!("{{ {bound} }} {lookup}")));
    assert_eq!(bounded, check(format!("{{ {bound} {lookup} }}")));
    assert_ne!(
        check(format!("{bound} {lookup} {lookup}")),
        check(format!("{lookup} {bound} {lookup}"))
    );
}

#[test]
fn only_the_visible_source_changes_a_family_entry() {
    for (body, changed, hidden) in [
        (
            "let x = first(); f.get_or_create(&labels!(x)); let x = second();",
            "first",
            "second",
        ),
        (
            "let x = first(); { let x = second(); f.get_or_create(&x); }",
            "second",
            "first",
        ),
        (
            "let x = first(); if let Some(x) = second() {} else { f.get_or_create(&x); }",
            "first",
            "second",
        ),
        (
            "let x = first(); match second() { Some(x) => f.get_or_create(&x), None => () }",
            "second",
            "first",
        ),
        (
            "let x = first(); for x in second() {} f.get_or_create(&x);",
            "first",
            "second",
        ),
        (
            "let x = first(); let x = f.get_or_create(&x); let x = second();",
            "first",
            "second",
        ),
        (
            "let x = first(); let x = second().x; f.get_or_create(&x);",
            "second",
            "first",
        ),
    ] {
        let check = |body: &str| {
            inventory(&parse_fixture(
                "metrics.rs",
                &format!("fn count() {{ {body} }}"),
            ))
        };
        let before = check(body);
        assert_ne!(before, check(&body.replace(changed, "third")), "{body}");
        assert_eq!(before, check(&body.replace(hidden, "third")), "{body}");
    }
}

/// Each form is pinned: the inventory has an entry for it, and changing the
/// marked text changes the inventory.
fn assert_pinned(cases: &[(&str, &str, &str, &str)]) {
    for (path, source, from, to) in cases {
        assert!(source.contains(from), "{source}: {from}");
        let before = inventory(&parse_fixture(path, source));
        assert_eq!(
            before.len(),
            1,
            "{path}: recorded once: {source}: {before:?}"
        );
        let after = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(before, after, "{path}: {from} -> {to}");
    }
}

#[test]
fn metric_calls_through_any_receiver_are_pinned() {
    assert_pinned(&[
        // An alias of the static.
        (
            "worker.rs",
            r#"use crate::metrics::METRICS as M; fn f() { M.observe_cache("one", "ok"); }"#,
            "one",
            "two",
        ),
        // A `&Metrics` binding.
        (
            "egress_upstream.rs",
            r#"fn f(metrics: &Metrics) { metrics.upstream_connection_lost(first); }"#,
            "first",
            "second",
        ),
        // A sink read from a thread-local.
        (
            "egress_upstream.rs",
            r#"fn f() { if let Some(sink) = SINK.get() { sink.upstream_connection_opened(first); } }"#,
            "first",
            "second",
        ),
        // A call through a field.
        (
            "worker.rs",
            r#"fn f(&self) { self.metrics.observe_cache("one", "ok"); }"#,
            "one",
            "two",
        ),
        // Inside a macro body.
        (
            "worker.rs",
            r#"fn f() { select! { _ = tick() => sink.observe_cache("one", "ok") } }"#,
            "one",
            "two",
        ),
        // Call form of a `Metrics` method, also inside a macro body.
        (
            "worker.rs",
            r#"fn f() { crate::metrics::Metrics::observe_cache(&METRICS, "one", "ok"); }"#,
            "one",
            "two",
        ),
        (
            "worker.rs",
            r#"fn f() { select! { _ = tick() => Metrics::observe_cache(&METRICS, "one", "ok") } }"#,
            "one",
            "two",
        ),
        // A `Metrics` method passed as a value.
        (
            "worker.rs",
            r#"fn f() { rows.into_iter().for_each(Metrics::observe_cache); }"#,
            "observe_cache",
            "observe_cache_n",
        ),
        // `self` inside `impl Metrics`.
        (
            "metrics/operations.rs",
            r#"impl Metrics { fn observe_cache(&self) { self.observe_cache_n("one", "ok", 1); } }"#,
            "one",
            "two",
        ),
        // Call form of a family access.
        (
            "metrics.rs",
            r#"fn count() { Family::get_or_create(&family, &[("kind", first)]); }"#,
            "first",
            "second",
        ),
        (
            "worker.rs",
            r#"fn count() { <Family<L, Counter>>::get_or_create_owned(&family, &[("kind", first)]); }"#,
            "first",
            "second",
        ),
    ]);
}

#[test]
fn a_call_of_another_type_named_like_a_metric_method_is_not_recorded() {
    // `self` outside `impl Metrics` is another type; a call without
    // arguments carries no label value.
    for source in [
        "impl Keys { fn f(&self) { self.refresh(store); } }",
        "fn f() { page.render(); }",
    ] {
        assert!(
            inventory(&parse_fixture("worker.rs", source)).is_empty(),
            "{source}"
        );
    }
}

#[test]
fn metric_method_names_are_read_from_the_tree() {
    let names = metric_methods();
    assert!(names.len() >= 90, "{} Metrics methods", names.len());
    for name in [
        "observe_attempt",
        "upstream_connection_lost",
        "upstream_connection_opened",
        "observe_http",
    ] {
        assert!(names.contains(name), "{name}");
    }
}

#[test]
fn resource_and_constant_labels_are_pinned() {
    assert_pinned(&[
        (
            "otlp.rs",
            r#"pub(crate) fn resource_attributes(v: &str) -> Value { json!([{"key":"service.name","value":{"stringValue":v}}]) }"#,
            "service.name",
            "host.name",
        ),
        (
            "telemetry.rs",
            r#"impl Exporter { fn body(&self) -> Vec<u8> { to_vec(&json!({"resource":{"attributes":[{"key":"service.name"}]}})) } }"#,
            "service.name",
            "host.name",
        ),
        (
            "metrics/registry.rs",
            r#"fn r() { let registry = Registry::with_labels([(first.into(), "x".into())].into_iter()); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r() { let registry = Registry::with_prefix_and_labels("gateway", labels(first)); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r(registry: &mut Registry) { registry.sub_registry_with_label(("host".into(), first)); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r(registry: &mut Registry) { registry.sub_registry_with_labels(labels(first)); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r(registry: &mut Registry) { registry.register("build", "", Info::new(vec![("version", first)])); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r(registry: &mut Registry) { registry.register_collector(Box::new(first)); }"#,
            "first",
            "second",
        ),
        (
            "metrics/custom.rs",
            r#"impl Collector for Host { fn encode(&self, e: DescriptorEncoder) -> Result { e.encode_descriptor(first) } }"#,
            "first",
            "second",
        ),
        (
            "metrics/labels.rs",
            r#"#[derive(Clone, EncodeLabelSet)] struct Labels { host: String }"#,
            "host",
            "client",
        ),
        (
            "metrics/labels.rs",
            r#"#[derive(EncodeLabelValue)] enum Kind { One }"#,
            "One",
            "Two",
        ),
        (
            "metrics/labels.rs",
            r#"#[cfg_attr(feature = "x", derive(EncodeLabelSet))] struct Labels { host: String }"#,
            "host",
            "client",
        ),
        (
            "metrics/labels.rs",
            r#"impl EncodeLabelSet for Labels { fn encode(&self, e: LabelSetEncoder) -> R { e.encode_label(first) } }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r() { let info = Info::new(vec![("version", first)]); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r() { select! { _ = t => registry.sub_registry_with_label(("host".into(), first)) } }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn r() { select! { _ = t => Info::new(vec![("version", first)]) } }"#,
            "first",
            "second",
        ),
        // The SELECT the attempt labels are read from.
        (
            "governance/settlement.rs",
            r#"async fn reservation(tx: &mut Tx<'_>) -> R { sqlx::query_as("SELECT e.provider,e.public_model FROM inference_executions e").fetch_one(tx).await }"#,
            "e.public_model",
            "e.upstream_model",
        ),
    ]);
}

#[test]
fn trace_span_attributes_are_pinned() {
    assert_pinned(&[
        (
            "telemetry.rs",
            "pub enum Attribute { Model(PublicAlias) }",
            "Model(PublicAlias)",
            "Model(PublicAlias), Host(String)",
        ),
        (
            "telemetry.rs",
            r#"impl Attribute { fn value(self) -> Option<Value> { Some(match self { Self::Model(v) => string("gen_ai.request.model", v.as_str()) }) } }"#,
            "gen_ai.request.model",
            "host.name",
        ),
        (
            "telemetry.rs",
            r#"impl Serialize for Span { fn serialize<S: Serializer>(&self, s: S) -> R { json!({"attributes":self.attributes}).serialize(s) } }"#,
            "self.attributes",
            "self.extra",
        ),
        (
            "telemetry.rs",
            r#"fixed_enum!(Provider { OpenAi => "openai" });"#,
            "openai",
            "open_ai",
        ),
        (
            "inference/request_telemetry.rs",
            "fn f(span: &mut Span) { span.attribute(telemetry::Attribute::Operation(first)); }",
            "first",
            "second",
        ),
        // An alias, a value bound first, and a macro body.
        (
            "inference/request_telemetry.rs",
            "use crate::telemetry::Attribute as A; fn f(span: &mut Span) { span.attribute(A::Model(first)); }",
            "first",
            "second",
        ),
        (
            "inference/request_telemetry.rs",
            "fn f(span: &mut Span) { span.attribute(first); }",
            "first",
            "second",
        ),
        (
            "inference/request_telemetry.rs",
            "fn f(span: &mut Span) { select! { _ = t => span.attribute(first) } }",
            "first",
            "second",
        ),
    ]);
}

#[test]
fn a_trailing_comma_does_not_change_an_entry_unless_it_changes_meaning() {
    let entries =
        |body: &str| inventory(&parse_fixture("worker.rs", &format!("fn f() {{ {body} }}")));
    // Lists whose trailing comma rustfmt adds or removes on a reflow.
    for (with, without) in [
        (
            r#"METRICS.observe_cache("one", "ok",);"#,
            r#"METRICS.observe_cache("one", "ok");"#,
        ),
        (
            r#"METRICS.observe_cache(kind(first,),);"#,
            r#"METRICS.observe_cache(kind(first));"#,
        ),
        (
            r#"METRICS.observe(match x { A => "a", B => "b", });"#,
            r#"METRICS.observe(match x { A => "a", B => "b" });"#,
        ),
        (
            r#"METRICS.observe(&[("kind", first),]);"#,
            r#"METRICS.observe(&[("kind", first)]);"#,
        ),
        (
            r#"METRICS.observe(Labels { kind: first, });"#,
            r#"METRICS.observe(Labels { kind: first });"#,
        ),
    ] {
        assert_eq!(entries(with), entries(without), "{with}");
    }
    // A one-element tuple is not a parenthesised value, and a macro may
    // match its trailing comma.
    for (with, without) in [
        ("METRICS.observe(&(first,));", "METRICS.observe(&(first));"),
        (
            "METRICS.observe(return (first,));",
            "METRICS.observe(return (first));",
        ),
        (
            "METRICS.observe(labels!(first,));",
            "METRICS.observe(labels!(first));",
        ),
    ] {
        assert_ne!(entries(with), entries(without), "{with}");
    }
    // A `macro_rules!` matcher in a body pinned by statement.
    let pinned = |rules: &str| {
        inventory(&parse_fixture(
            "metrics.rs",
            &format!("fn bounded() {{ {rules} }}"),
        ))
    };
    assert_ne!(
        pinned("macro_rules! m { [$a:expr,] => {} }"),
        pinned("macro_rules! m { [$a:expr] => {} }")
    );
}

#[test]
fn metric_method_names_are_specific() {
    // A call of a `Metrics` method on any receiver is pinned, so a generic
    // name would pin every unrelated call of that name.
    for generic in [
        "add", "call", "clear", "finish", "get", "inc", "insert", "observe", "push", "record",
        "remove", "run", "send", "set", "start", "update",
    ] {
        assert!(!metric_methods().contains(generic), "Metrics::{generic}");
    }
}

#[test]
fn spellings_of_a_metric_call_are_pinned() {
    assert_pinned(&[
        (
            "egress_upstream.rs",
            "fn f(m: &Metrics) { m.r#upstream_connection_lost(first); }",
            "first",
            "second",
        ),
        (
            "egress_upstream.rs",
            "fn f(m: &Metrics) { <Metrics>::upstream_connection_lost(m, first); }",
            "first",
            "second",
        ),
        (
            "egress_upstream.rs",
            "fn f(m: &Metrics) { <Metrics as Emit>::upstream_connection_lost(m, first); }",
            "first",
            "second",
        ),
        (
            "egress_upstream.rs",
            "use crate::metrics::Metrics as M; fn f() { M::upstream_connection_lost(&METRICS, first); }",
            "first",
            "second",
        ),
        (
            "egress_upstream.rs",
            "type M = crate::metrics::Metrics; fn f() { M::upstream_connection_lost(&METRICS, first); }",
            "first",
            "second",
        ),
        (
            "egress_upstream.rs",
            "fn f(m: &Metrics) { let _ = matches!({ (*m).upstream_connection_lost(first); }, ()); }",
            "first",
            "second",
        ),
        (
            "inference/request_telemetry.rs",
            "use crate::telemetry::Attribute::Streamed; fn f(span: &mut Span) { span.attribute(Streamed(first)); }",
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"use prometheus_client::metrics::info::Info as Metadata; fn r(registry: &mut Registry) { registry.register("p", "", Metadata::new(vec![("v", first)])); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"type Meta = Info<Vec<(&'static str, String)>>; fn r() { let _ = Meta::new(vec![("v", first)]); }"#,
            "first",
            "second",
        ),
    ]);
}

#[test]
fn every_change_of_an_otlp_attribute_list_is_pinned() {
    assert_pinned(&[
        (
            "metrics_export.rs",
            r#"impl Encoder { fn envelope(&self) -> Value { json!({"resourceMetrics":[{"resource":{"attributes":first}}]}) } }"#,
            "first",
            "second",
        ),
        (
            "metrics_export.rs",
            r#"impl Encoder { fn encode(&self) { let mut e = self.envelope(); e["resourceMetrics"][0]["resource"]["attributes"].as_array_mut().unwrap().push(first); } }"#,
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"impl Exporter { fn body(&self, records: &[Value]) -> Vec<u8> { to_vec(&self.envelope(records)).expect(first) } }"#,
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"fn batch(records: &mut [Value]) { for r in records { r["attributes"].as_array_mut().unwrap().push(first); } }"#,
            "first",
            "second",
        ),
        (
            "telemetry.rs",
            r#"fn spans(span: &mut Value) { span["attributes"] = first; }"#,
            "first",
            "second",
        ),
        (
            "otlp.rs",
            r#"fn extra() -> Value { json!({"attributes":[first]}) }"#,
            "first",
            "second",
        ),
    ]);
}

#[test]
fn a_comma_a_macro_may_match_changes_the_entry() {
    let entries = |call: &str| {
        inventory(&parse_fixture(
            "worker.rs",
            &format!("fn f(m: &Metrics) {{ label!({call}); }}"),
        ))
    };
    assert_ne!(
        entries(r#"m.upstream_connection_lost("probe",)"#),
        entries(r#"m.upstream_connection_lost("probe")"#)
    );
}
