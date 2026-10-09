//! Shared portable JSON Schema subset used by provider tool contracts and host validation.
use serde_json::Value;

pub(crate) fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let d = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object"))?;
    if let Some(alternatives) = d.get("anyOf") {
        let alternatives = alternatives
            .as_array()
            .ok_or_else(|| format!("{path}: anyOf must be an array"))?;
        let mut branch_errors = Vec::new();
        let mut matched = false;
        for alternative in alternatives {
            match validate(value, alternative, path) {
                Ok(()) => {
                    matched = true;
                    break;
                }
                Err(error) if !branch_errors.contains(&error) => branch_errors.push(error),
                Err(_) => {}
            }
        }
        if !matched {
            let details = branch_errors
                .iter()
                .take(8)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("{path}: no anyOf schema matched ({details})"));
        }
    }
    if let Some(t) = d.get("type") {
        let is = |k: &str| match k {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => {
                value.as_i64().is_some()
                    || value.as_u64().is_some()
                    || value.as_f64().is_some_and(|number| number.fract() == 0.0)
            }
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        let ok = match t {
            Value::String(k) => is(k),
            Value::Array(a) => a.iter().filter_map(Value::as_str).any(is),
            _ => false,
        };
        if !ok {
            return Err(format!("{path}: type mismatch"));
        }
    }
    if let Some(e) = d.get("enum").and_then(Value::as_array) {
        if !e.contains(value) {
            return Err(format!("{path}: enum mismatch"));
        }
    }
    if let Some(o) = value.as_object() {
        if let Some(req) = d.get("required").and_then(Value::as_array) {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    return Err(format!("{path}.{k}: required"));
                }
            }
        }
        let props = d.get("properties").and_then(Value::as_object);
        for (k, v) in o {
            if let Some(s) = props.and_then(|p| p.get(k)) {
                validate(v, s, &format!("{path}.{k}"))?
            } else if d.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("{path}.{k}: additional property"));
            } else if let Some(s) = d.get("additionalProperties").filter(|x| x.is_object()) {
                validate(v, s, &format!("{path}.{k}"))?
            }
        }
    }
    if let Some(a) = value.as_array() {
        if d.get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|n| a.len() < n as usize)
        {
            return Err(format!("{path}: minItems"));
        }
        if d.get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|n| a.len() > n as usize)
        {
            return Err(format!("{path}: maxItems"));
        }
        if let Some(s) = d.get("items") {
            for (i, v) in a.iter().enumerate() {
                validate(v, s, &format!("{path}[{i}]"))?
            }
        }
    }
    if let Some(s) = value.as_str() {
        let n = s.chars().count() as u64;
        if d.get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|x| n < x)
        {
            return Err(format!("{path}: minLength"));
        }
        if d.get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|x| n > x)
        {
            return Err(format!("{path}: maxLength"));
        }
        if let Some(p) = d.get("pattern").and_then(Value::as_str) {
            let re = regex::Regex::new(p).map_err(|e| format!("{path}: invalid pattern: {e}"))?;
            if !re.is_match(s) {
                return Err(format!("{path}: pattern"));
            }
        }
    }
    if let Some(n) = value.as_f64() {
        if d.get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|x| n < x)
        {
            return Err(format!("{path}: minimum"));
        }
        if d.get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|x| n > x)
        {
            return Err(format!("{path}: maximum"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_nested_portable_schema() {
        let s = json!({"type":"object","properties":{"paths":{"type":"array","minItems":1,"maxItems":2,"items":{"type":"string","pattern":"^[^.]+$"}},"mode":{"type":"string","enum":["safe"]}},"required":["paths","mode"],"additionalProperties":false});
        assert!(validate(&json!({"paths":["src"],"mode":"safe"}), &s, "$").is_ok());
        assert!(validate(&json!({"paths":["src","a","b"],"mode":"safe"}), &s, "$").is_err());
        assert!(validate(
            &json!({"paths":["src"],"mode":"safe","extra":true}),
            &s,
            "$"
        )
        .is_err());
    }
    #[test]
    fn validates_one_of_multiple_contract_shapes_with_any_of() {
        let schema = json!({
            "anyOf": [
                {"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},
                {"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}
            ]
        });
        assert!(validate(&json!({"text":"hello"}), &schema, "$").is_ok());
        assert!(validate(&json!({"command":"npm test"}), &schema, "$").is_ok());
        assert!(validate(&json!({"text":"hello","command":"npm test"}), &schema, "$").is_err());
        assert!(validate(&json!({}), &schema, "$").is_err());
    }

    #[test]
    fn supports_type_unions_and_numeric_bounds() {
        let s = json!({"type":["integer","null"],"minimum":1,"maximum":4});
        assert!(validate(&json!(null), &s, "$").is_ok());
        assert!(validate(&json!(3.0), &s, "$").is_ok());
        assert!(validate(&json!(5), &s, "$").is_err());
    }
}
