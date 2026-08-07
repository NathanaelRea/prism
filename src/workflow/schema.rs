use serde_json::Value;

/// Validate a value against the JSON Schema subset accepted for Workflow artifacts.
pub(crate) fn validate_value(value: &Value, schema: &Value) -> Result<(), String> {
    validate_at(value, schema, "value")
}

fn validate_at(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let valid = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "null" => value.is_null(),
            _ => true,
        };
        if !valid {
            return Err(format!("{path} must be a JSON {expected}"));
        }
    }

    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }

    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}.{name} is required"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, child_schema) in properties {
                if let Some(child) = object.get(name) {
                    validate_at(child, child_schema, &format!("{path}.{name}"))?;
                }
            }
        }
    }

    if let Some(array) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && array.len() < minimum as usize
        {
            return Err(format!("{path} needs at least {minimum} item(s)"));
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                validate_at(item, item_schema, &format!("{path}[{index}]"))?;
            }
        }
    }

    if let Some(text) = value.as_str()
        && let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
        && text.chars().count() < minimum as usize
    {
        return Err(format!("{path} must not be empty"));
    }

    if let Some(alternatives) = schema.get("anyOf").and_then(Value::as_array)
        && !alternatives
            .iter()
            .any(|alternative| validate_at(value, alternative, path).is_ok())
    {
        return Err(format!("{path} does not satisfy any allowed input shape"));
    }
    if let Some(alternatives) = schema.get("oneOf").and_then(Value::as_array)
        && alternatives
            .iter()
            .filter(|alternative| validate_at(value, alternative, path).is_ok())
            .count()
            != 1
    {
        return Err(format!(
            "{path} must satisfy exactly one allowed input shape"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_nested_required_and_type_errors() {
        let schema = json!({
            "type":"object",
            "required":["title"],
            "properties":{"title":{"type":"string", "minLength":1}}
        });
        assert_eq!(
            validate_value(&json!({}), &schema).unwrap_err(),
            "value.title is required"
        );
        assert_eq!(
            validate_value(&json!({"title":2}), &schema).unwrap_err(),
            "value.title must be a JSON string"
        );
    }
}
