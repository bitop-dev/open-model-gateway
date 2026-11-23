//! JSON preflight without building a value tree and small, explicit mapper
//! budgets over the fields the mapper reads. Refusing the whole draft avoids
//! presenting a truncated prefix as a conservative rate.
use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

pub(super) const MAX_ITEMS: usize = 64;
pub(super) const MAX_WARNINGS: usize = 64;
/// Endpoints compared for one model. Endpoints add no draft lines; the
/// comparison is linear in the list and in the at most 64 evidence rows.
pub(super) const MAX_ENDPOINTS: usize = 256;
/// Distinct warnings endpoint selection can add after `map` (one each):
/// the catalog fallback, an unusable ceiling, untranslated tiers, excluded
/// incomplete rates, an excluded unreadable status, evidence on an endpoint
/// not reported up, the endpoint priced from, a zero price and a pricier
/// endpoint. `map` leaves them room, so they are never omitted.
pub(super) const SELECTION_WARNINGS: usize = 9;
pub(super) const MAPPER_WARNINGS: usize = MAX_WARNINGS - SELECTION_WARNINGS;
const MAPPING_NODES: usize = 4096;
const MAPPING_TEXT: usize = 16_384;
const MAPPING_DEPTH: usize = 64;
const CATALOG_NODES: usize = 131_072;
const CATALOG_TEXT: usize = 16_384;
// Classify nesting refusals before serde_json reaches its recursion guard.
const CATALOG_DEPTH: usize = 96;
/// Model fields `map` reads besides `top_provider.max_completion_tokens`.
const MODEL_FIELDS: [&str; 3] = ["id", "context_length", "pricing"];
/// Endpoint fields selection and `map` read.
const ENDPOINT_FIELDS: [&str; 6] = [
    "pricing",
    "status",
    "provider_name",
    "tag",
    "context_length",
    "max_completion_tokens",
];

struct Budget {
    nodes: usize,
    items: usize,
    text: usize,
    depth: usize,
    exceeded: bool,
}

impl Budget {
    fn reject_limit<E: serde::de::Error>(&mut self, message: &'static str) -> E {
        self.exceeded = true;
        E::custom(message)
    }
}
struct Check<'a>(&'a mut Budget, usize);
impl<'de> DeserializeSeed<'de> for Check<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        if self.1 > self.0.depth {
            return Err(self.0.reject_limit("catalog depth limit"));
        }
        if self.0.nodes == 0 {
            return Err(self.0.reject_limit("catalog complexity limit"));
        }
        self.0.nodes -= 1;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Check<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded catalog JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<(), E> {
        if s.len() > self.0.text {
            return Err(self.0.reject_limit("catalog text limit"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0;
        while a.next_element_seed(Check(self.0, self.1 + 1))?.is_some() {
            n += 1;
            if n > self.0.items {
                return Err(self.0.reject_limit("catalog item limit"));
            }
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0;
        while let Some(key) = a.next_key_seed(KeyLen)? {
            n += 1;
            if n > self.0.items || key > self.0.text {
                return Err(self.0.reject_limit("catalog field limit"));
            }
            a.next_value_seed(Check(self.0, self.1 + 1))?;
        }
        Ok(())
    }
}
/// A key's length in bytes. serde_json hands over a borrowed slice of the body
/// or of its reusable unescaping buffer, so no key is copied.
struct KeyLen;
impl<'de> DeserializeSeed<'de> for KeyLen {
    type Value = usize;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<usize, D::Error> {
        d.deserialize_str(self)
    }
}
impl<'de> Visitor<'de> for KeyLen {
    type Value = usize;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<usize, E> {
        Ok(s.len())
    }
}
fn check_json(body: &[u8], budget: &mut Budget) -> Result<(), CatalogError> {
    if body.len() > BODY_CAP {
        return Err(CatalogError::Limit);
    }
    let mut d = serde_json::Deserializer::from_slice(body);
    Check(budget, 0).deserialize(&mut d).map_err(|_| {
        if budget.exceeded {
            CatalogError::Limit
        } else {
            CatalogError::Unavailable
        }
    })?;
    d.end().map_err(|_| CatalogError::Unavailable)
}
pub(super) fn catalog_json(body: &[u8]) -> Result<(), CatalogError> {
    check_json(
        body,
        &mut Budget {
            nodes: CATALOG_NODES,
            items: CATALOG_NODES,
            text: CATALOG_TEXT,
            depth: CATALOG_DEPTH,
            exceeded: false,
        },
    )
}
pub(super) fn mapping_json(body: &[u8]) -> bool {
    check_json(
        body,
        &mut Budget {
            nodes: MAPPING_NODES,
            items: MAX_ITEMS,
            text: MAPPING_TEXT,
            depth: MAPPING_DEPTH,
            exceeded: false,
        },
    )
    .is_ok()
}
/// Charges `v` against the mapping budget: 64 entries per array or object,
/// 16 KiB per string or key, a depth limit, and `remaining` values in total.
fn charge(v: &Value, remaining: &mut usize, depth: usize) -> bool {
    if *remaining == 0 || depth > MAPPING_DEPTH {
        return false;
    }
    *remaining -= 1;
    match v {
        Value::String(s) => s.len() <= MAPPING_TEXT,
        Value::Array(a) => {
            a.len() <= MAX_ITEMS && a.iter().all(|v| charge(v, remaining, depth + 1))
        }
        Value::Object(o) => {
            o.len() <= MAX_ITEMS
                && o.iter()
                    .all(|(k, v)| k.len() <= MAPPING_TEXT && charge(v, remaining, depth + 1))
        }
        _ => true,
    }
}
/// Copies the named fields of `from` that pass the budget, or `None`. Only
/// these fields are traversed; anything else in the object is never read.
fn copy_fields(
    from: &serde_json::Map<String, Value>,
    fields: &[&str],
    remaining: &mut usize,
    depth: usize,
) -> Option<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    for key in fields {
        if let Some(v) = from.get(*key) {
            if !charge(v, remaining, depth) {
                return None;
            }
            out.insert((*key).to_owned(), v.clone());
        }
    }
    Some(out)
}
/// The catalog model reduced to what `map` reads (`id`, `context_length`,
/// `top_provider.max_completion_tokens` and the whole `pricing` tree), or
/// `None` when those fields exceed the mapping budget. Other fields (voice
/// lists, parameters, descriptions) are bounded only by `catalog_json` and
/// never reach the mapper. A value that is not an object has no fields.
pub(super) fn model_fields(model: &Value) -> Option<Value> {
    let Some(model) = model.as_object() else {
        return Some(json!({}));
    };
    let mut remaining = MAPPING_NODES;
    let mut out = copy_fields(model, &MODEL_FIELDS, &mut remaining, 1)?;
    if let Some(top) = model.get("top_provider").and_then(Value::as_object) {
        let top = copy_fields(top, &["max_completion_tokens"], &mut remaining, 2)?;
        out.insert("top_provider".into(), Value::Object(top));
    }
    Some(Value::Object(out))
}
/// The endpoint list selection compares (`data.endpoints`, else `endpoints`;
/// an empty or missing list is `Ok(None)`), each endpoint reduced to the
/// fields that are read. `Err` when the list or those fields exceed the
/// budget: at most 256 endpoints and 4,096 read values for the document.
pub(super) fn endpoint_list(doc: &Value) -> Result<Option<Vec<Value>>, ()> {
    let Some(list) = doc["data"]["endpoints"]
        .as_array()
        .or(doc["endpoints"].as_array())
        .filter(|l| !l.is_empty())
    else {
        return Ok(None);
    };
    if list.len() > MAX_ENDPOINTS {
        return Err(());
    }
    let mut remaining = MAPPING_NODES;
    list.iter()
        .map(|endpoint| {
            if remaining == 0 {
                return Err(());
            }
            remaining -= 1;
            // A non-object entry has no readable fields, as when indexed.
            let Some(endpoint) = endpoint.as_object() else {
                return Ok(json!({}));
            };
            copy_fields(endpoint, &ENDPOINT_FIELDS, &mut remaining, 2)
                .map(Value::Object)
                .ok_or(())
        })
        .collect::<Result<Vec<_>, ()>>()
        .map(Some)
}

/// No prefix rates survive a complexity refusal, including request fees and
/// not-applicable assertions. The operator must enter and review all rates.
pub(super) fn refused(model: &Value, workload: WorkloadKind) -> Value {
    // Keep the normal workload ceiling rules without traversing the refused
    // pricing data. Only bounded scalar metadata can reach the mapper here.
    let metadata = json!({
        "id": model["id"].as_str().filter(|s| s.len() <= 200),
        "context_length": model["context_length"].as_u64(),
        "top_provider": {"max_completion_tokens": model["top_provider"]["max_completion_tokens"].as_u64()},
    });
    let mut out = map(&metadata, workload, None);
    let unknown: Vec<_> = Meter::ALL
        .iter()
        .map(|m| json!({"meter":m.as_str(),"unknown":true}))
        .collect();
    out["lines"] = json!(Meter::ALL.iter().map(|m| json!({"meter":m.as_str(),"microusd_per_batch":null,"needs_review":true,"source":"manual"})).collect::<Vec<_>>());
    out["draft"]["price_lines"] = json!(unknown);
    out["display_lines"] = Value::Null;
    out["warnings"] = json!([
        "Catalog pricing exceeds safe draft limits; no prices were imported. Enter and review the rates manually.",
        "The draft lines failed validation; edit them before publishing."
    ]);
    out["needs_review"] = json!(true);
    out
}
