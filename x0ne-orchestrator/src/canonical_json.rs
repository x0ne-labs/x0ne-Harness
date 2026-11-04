//! Keys sorted at every depth, whatever serde_json's `preserve_order`.
//! That feature is workspace-wide once any dependency enables it
//! (agent-client-protocol does, 2026-10-06): sorted output must be explicit.

use serde_json::Value;
use std::collections::BTreeMap;

pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let sorted: BTreeMap<&String, Value> =
                o.iter().map(|(k, x)| (k, canonical(x))).collect();
            Value::Object(sorted.into_iter().map(|(k, x)| (k.clone(), x)).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_key_order_does_not_change_the_bytes() {
        let a: Value = serde_json::from_str(r#"{"b":{"y":1,"x":[{"q":1,"p":2}]},"a":0}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":0,"b":{"x":[{"p":2,"q":1}],"y":1}}"#).unwrap();
        assert_eq!(canonical(&a).to_string(), canonical(&b).to_string());
        assert_eq!(
            canonical(&a).to_string(),
            r#"{"a":0,"b":{"x":[{"p":2,"q":1}],"y":1}}"#
        );
    }
}
