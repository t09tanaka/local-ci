//! `strategy.matrix` expansion, following GitHub's rules for `include` / `exclude`.

use anyhow::{bail, Result};
use serde_json::{Map, Value};

pub fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Expand a matrix (already a JSON object) into its combinations.
/// An absent matrix is one empty combination.
pub fn expand(m: &Value) -> Result<Vec<Map<String, Value>>> {
    let obj = match m {
        Value::Null => return Ok(vec![Map::new()]),
        Value::Object(o) => o,
        other => bail!("matrix is not a mapping: {other}"),
    };
    let mut combos: Vec<Map<String, Value>> = vec![Map::new()];
    let mut original_keys = Vec::new();
    for (k, v) in obj {
        if k == "include" || k == "exclude" {
            continue;
        }
        let values = match v {
            Value::Array(a) => a.clone(),
            Value::String(s) if s.contains("${{") => {
                bail!("matrix key `{k}` is an unevaluated expression")
            }
            other => vec![other.clone()],
        };
        original_keys.push(k.clone());
        let mut next = Vec::new();
        for c in &combos {
            for val in &values {
                let mut c2 = c.clone();
                c2.insert(k.clone(), val.clone());
                next.push(c2);
            }
        }
        combos = next;
    }
    let had_base = !original_keys.is_empty();
    if !had_base {
        combos.clear();
    }
    if let Some(Value::Array(ex)) = obj.get("exclude") {
        combos.retain(|c| {
            !ex.iter().any(|e| {
                e.as_object()
                    .is_some_and(|e| e.iter().all(|(k, v)| c.get(k) == Some(v)))
            })
        });
    }
    if let Some(Value::Array(inc)) = obj.get("include") {
        let base_len = combos.len();
        for item in inc {
            let Some(item) = item.as_object() else {
                continue;
            };
            let mut matched = false;
            for c in combos.iter_mut().take(base_len) {
                // an include entry extends every combination whose original values it does not change
                let compatible = item
                    .iter()
                    .all(|(k, v)| !original_keys.contains(k) || c.get(k) == Some(v));
                if compatible && had_base {
                    for (k, v) in item {
                        c.insert(k.clone(), v.clone());
                    }
                    matched = true;
                }
            }
            if !matched {
                combos.push(item.clone());
            }
        }
    }
    if combos.is_empty() {
        bail!("matrix has no combinations");
    }
    Ok(combos)
}

/// "integration, 1/2" for display.
pub fn describe(c: &Map<String, Value>) -> String {
    c.values()
        .map(crate::expr::to_string)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cartesian_exclude_include() {
        let m = json!({"os": ["a", "b"], "node": [20, 22], "exclude": [{"os": "b", "node": 20}], "include": [{"os": "a", "extra": true}, {"os": "c", "node": 18}]});
        let c = expand(&m).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(
            c[0],
            json!({"os": "a", "node": 20, "extra": true})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(c[1].get("extra"), Some(&json!(true)));
        assert_eq!(
            c[2],
            json!({"os": "b", "node": 22}).as_object().unwrap().clone()
        );
        assert_eq!(
            c[3],
            json!({"os": "c", "node": 18}).as_object().unwrap().clone()
        );
    }

    #[test]
    fn include_only() {
        let m = json!({"include": [{"name": "int 1/2", "shard": "1/2"}, {"name": "e2e", "shard": "all"}]});
        let c = expand(&m).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(describe(&c[1]), "e2e, all");
    }

    #[test]
    fn empty_and_errors() {
        assert_eq!(expand(&Value::Null).unwrap(), vec![Map::new()]);
        assert!(expand(&json!({"project": []})).is_err());
        assert!(expand(&json!("x")).is_err());
    }
}
