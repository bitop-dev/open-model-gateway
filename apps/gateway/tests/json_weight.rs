//! The pre-parse structure estimate of a JSON body (`inference::json_weight`,
//! #872), added to the body's length, is never below what parsing it
//! allocates. This binary counts every
//! allocation of the thread that parses (a counting global allocator, which
//! is why it is its own test binary), and compares the peak of live bytes
//! during `serde_json` parses of generated and adversarial bodies with the
//! estimate of the same bytes.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use open_model_gateway::inference::json_weight::estimate;
use proptest::prelude::*;
use serde_json::{Value, json};

struct Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn count(delta: isize) {
    let _ = ON.try_with(|on| {
        if on.get() {
            let live = LIVE.get() + delta;
            LIVE.set(live);
            PEAK.set(PEAK.get().max(live));
        }
    });
}

// SAFETY: every call is forwarded to `System` unchanged; the wrapper only
// adds the sizes to thread-local counters, which allocate nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc`, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            count(layout.size().cast_signed());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc_zeroed`, passed on.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(layout.size().cast_signed());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc`, passed on.
        unsafe { System.dealloc(p, layout) };
        count(-layout.size().cast_signed());
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc`, passed on.
        let q = unsafe { System.realloc(p, layout, new) };
        if !q.is_null() {
            // Counted as a new block beside the old one, then the old one
            // freed: a move holds both for a moment.
            count(new.cast_signed());
            count(-layout.size().cast_signed());
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The peak of bytes the parse of `body` held at once, the parsed value
/// included, and whether it parsed.
fn parse_peak(body: &[u8]) -> (usize, bool) {
    LIVE.set(0);
    PEAK.set(0);
    ON.set(true);
    let parsed = serde_json::from_slice::<Value>(body);
    ON.set(false);
    let peak = usize::try_from(PEAK.get()).unwrap();
    let ok = parsed.is_ok();
    drop(parsed);
    (peak, ok)
}

fn holds(body: &[u8]) -> Result<(), String> {
    let (peak, _) = parse_peak(body);
    let estimate = body.len() + estimate(body);
    if estimate >= peak {
        Ok(())
    } else {
        Err(format!(
            "estimate {estimate} below the parse's peak {peak} for a body of {} bytes: {}",
            body.len(),
            String::from_utf8_lossy(&body[..body.len().min(200)])
        ))
    }
}

/// Every array and object length around the points where a vector or a
/// B-tree reallocates or splits, with each kind of element.
#[test]
fn the_estimate_covers_every_growth_point() {
    let elements: [&str; 12] = [
        "0",
        "-1.5e300",
        "true",
        "null",
        "\"\"",
        "\"a\"",
        "[]",
        "[0]",
        "{}",
        "{\"a\":0}",
        "\"\\u00e9\"",
        "[[0]]",
    ];
    for element in elements {
        for n in (0..=70).chain([127, 128, 129, 255, 256, 257, 1023, 1024, 1025, 4097]) {
            let array = format!("[{}]", vec![element; n].join(","));
            holds(array.as_bytes()).unwrap();
            let object = format!(
                "{{{}}}",
                (0..n)
                    .map(|k| format!("\"{k:06}\":{element}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            holds(object.as_bytes()).unwrap();
            // Keys in descending order split the B-tree differently.
            let reversed = format!(
                "{{{}}}",
                (0..n)
                    .rev()
                    .map(|k| format!("\"{k:06}\":{element}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            holds(reversed.as_bytes()).unwrap();
        }
    }
}

/// Shapes built to be large parsed: tiny containers, one-key objects,
/// nesting to `serde_json`'s depth limit, escapes that shrink when decoded,
/// duplicate keys, every number form, whitespace, and invalid JSON.
#[test]
fn the_estimate_covers_hostile_shapes() {
    let n = 20_000;
    let repeat = |item: &str| format!("[{}]", vec![item; n].join(","));
    let mut bodies = vec![
        repeat("0"),
        repeat("[]"),
        repeat("[0]"),
        repeat("[[0]]"),
        repeat("{}"),
        repeat("{\"a\":0}"),
        repeat("{\"\":{}}"),
        repeat("{\"a\":[0]}"),
        repeat("\"\\u0000\""),
        repeat("\"\\ud83d\\ude00\""),
        repeat("{\"\\u0061\":0}"),
        repeat("-0.0e-0"),
        repeat("18446744073709551615"),
        repeat("-9223372036854775808"),
        repeat("1e400"),
        format!("[{}]", vec!["1"; n].join(" , \n\t")),
        format!("{{{}}}", vec!["\"a\":0"; n].join(",")),
        format!("{}{}", "[".repeat(127), "]".repeat(127)),
        format!("{}0{}", "[".repeat(127), "]".repeat(127)),
        format!(
            "{}{}",
            "{\"a\":".repeat(127),
            format!("0{}", "}".repeat(127))
        ),
        format!("[{}", vec!["{\"a\":0}"; n].join(",")),
        format!("[{},\"\u{1}\"]", vec!["{\"a\":0}"; n].join(",")),
        format!("[{},]", vec!["[0]"; n].join(",")),
        format!("[{} 0]", vec!["[0]"; n].join(",")),
    ];
    bodies.push(
        serde_json::to_string(&json!({"a": vec![json!({"b": [1, 2, {"c": null}]}); n]})).unwrap(),
    );
    // Long strings with an escape: decoded through a reused scratch buffer
    // that grows to twice the longest, before the string is copied out.
    let long = "x".repeat(1 << 20);
    bodies.extend([
        format!(r#"["\n{long}"]"#),
        format!(r#"["{long}\n"]"#),
        format!(r#"{{"{long}\n":0}}"#),
        format!(r#"["data:image/png;base64,{}\/"]"#, "A".repeat(1 << 20)),
        format!(
            r#"["{long}\n","{}\n","{}\n"]"#,
            &long[..1000],
            &long[..(1 << 19)]
        ),
        format!(r#"["{}é"]"#, "é".repeat(1 << 18)),
        format!(r#"["{}","{long}\n"]"#, r"\n".repeat(1 << 18)),
    ]);
    for body in bodies {
        holds(body.as_bytes()).unwrap();
    }
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| json!(f)),
        ".{0,12}".prop_map(Value::String),
        "[\\x00-\\x1f\"\\\\/]{0,6}".prop_map(Value::String),
        // Long strings with escapes anywhere in them.
        (
            "[a-z]{0,1500}",
            "[\\x00-\\x1f\"\\\\/]{0,3}",
            "[a-z]{0,1500}"
        )
            .prop_map(|(a, e, b)| Value::String(format!("{a}{e}{b}"))),
    ];
    leaf.prop_recursive(8, 400, 40, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..40).prop_map(Value::Array),
            prop::collection::vec((".{0,6}", inner), 0..40)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Any JSON, compact or pretty, with `\/` and `\uXXXX` escapes written
    /// where an encoder may: the estimate is at least the parse's peak.
    #[test]
    fn the_estimate_is_never_below_the_parse(value in value(), pretty in any::<bool>(), escape in any::<bool>()) {
        let mut body = if pretty {
            serde_json::to_string_pretty(&value).unwrap()
        } else {
            serde_json::to_string(&value).unwrap()
        };
        if escape {
            body = body.replace('/', "\\/").replace('a', "\\u0061");
        }
        prop_assert!(holds(body.as_bytes()).is_ok(), "{}", holds(body.as_bytes()).unwrap_err());
    }
}

/// The measured cost of each element shape, beside what the estimate
/// charges for it (run with `--ignored --nocapture` when revisiting the
/// constants).
#[test]
#[ignore = "manual calibration of inference::json_weight's constants"]
fn calibration() {
    for item in [
        "0",
        "\"a\"",
        "[]",
        "[0]",
        "{}",
        "{\"a\":0}",
        "{\"a\":{\"b\":0}}",
        "[[0]]",
    ] {
        for n in [1000, 1025, 20_000, 32_769] {
            let body = format!("[{}]", vec![item; n].join(","));
            let (peak, _) = parse_peak(body.as_bytes());
            let over_bytes = peak.saturating_sub(body.len());
            println!(
                "{item:16} n={n:6}  parse {:7.1} B/item beyond the bytes, estimate {:7.1} B/item",
                over_bytes as f64 / n as f64,
                estimate(body.as_bytes()) as f64 / n as f64
            );
        }
    }
}

/// The walk's time per MiB on text, on dense structure and on long strings
/// (run in a release build with `--ignored --nocapture`).
#[test]
#[ignore = "manual measurement of the walk's cost"]
fn timing() {
    let mib = 1024 * 1024;
    let text = serde_json::to_vec(
        &json!({"messages":[{"role":"user","content":"word ".repeat(2 * mib / 5)}]}),
    )
    .unwrap();
    let numbers = format!("[{}]", vec!["0"; mib].join(",")).into_bytes();
    let objects = format!("[{}]", vec!["{\"a\":0}"; mib / 4].join(",")).into_bytes();
    for (name, body) in [("text", text), ("numbers", numbers), ("objects", objects)] {
        let runs = 50;
        let start = std::time::Instant::now();
        let mut total = 0;
        for _ in 0..runs {
            total += std::hint::black_box(estimate(std::hint::black_box(&body)));
        }
        let size = body.len() as f64 / mib as f64;
        let per_mib = start.elapsed().as_secs_f64() * 1e6 / runs as f64 / size;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            drop(std::hint::black_box(serde_json::from_slice::<Value>(
                std::hint::black_box(&body),
            )));
        }
        let parse = start.elapsed().as_secs_f64() * 1e6 / runs as f64 / size;
        println!(
            "{name:8} {size:.2} MiB: walk {per_mib:7.0} µs per MiB, serde_json parse {parse:7.0} µs per MiB ({total})"
        );
    }
}
