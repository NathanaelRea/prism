pub fn json_escape(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&encoded)
        .to_string()
}

pub fn json_string_field(text: &str, key: &str) -> Option<String> {
    json_field(text, key).and_then(|value| value.as_str().map(str::to_string))
}

pub fn json_u64_field(text: &str, key: &str) -> Option<u64> {
    json_field(text, key).and_then(|value| value.as_u64())
}

pub fn json_bool_field(text: &str, key: &str) -> Option<bool> {
    json_field(text, key).and_then(|value| value.as_bool())
}

fn json_field(text: &str, key: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    find_json_field(&value, key).cloned()
}

fn find_json_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(object) => object.get(key).or_else(|| {
            object
                .values()
                .find_map(|value| find_json_field(value, key))
        }),
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_json_field(value, key))
        }
        _ => None,
    }
}

pub fn json_object_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)?;
    let rest = &text[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if !value.starts_with('{') {
        return None;
    }
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&value[..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn json_top_level_objects(text: &str) -> Vec<&str> {
    let mut objects = Vec::new();
    let mut start = None;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0
                    && let Some(object_start) = start.take()
                {
                    objects.push(&text[object_start..=index]);
                }
            }
            _ => {}
        }
    }
    objects
}

#[cfg(test)]
#[allow(dead_code)]
pub fn json_login_field(text: &str) -> Option<String> {
    json_string_field(text, "login")
        .or_else(|| json_string_field(text, "author"))
        .or_else(|| json_string_field(text, "user"))
}

#[cfg(test)]
#[allow(dead_code)]
pub fn collect_json_string_fields(text: &str, key: &str, limit: usize) -> Vec<String> {
    let mut values = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        collect_json_string_values(&value, key, limit, &mut values);
    }
    values
}

#[cfg(test)]
#[allow(dead_code)]
fn collect_json_string_values(
    value: &serde_json::Value,
    key: &str,
    limit: usize,
    values: &mut Vec<String>,
) {
    if values.len() >= limit {
        return;
    }
    match value {
        serde_json::Value::Object(object) => {
            if let Some(value) = object.get(key).and_then(|value| value.as_str())
                && !value.trim().is_empty()
            {
                values.push(value.to_string());
            }
            for value in object.values() {
                collect_json_string_values(value, key, limit, values);
                if values.len() >= limit {
                    break;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                collect_json_string_values(value, key, limit, values);
                if values.len() >= limit {
                    break;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_field_reads_escaped_summary() {
        let value = json_string_field(
            r#"{"prompt_summary":"fix \"review\" comments","other":"ignored"}"#,
            "prompt_summary",
        );
        assert_eq!(value.as_deref(), Some("fix \"review\" comments"));
    }

    #[test]
    fn json_helpers_parse_basic_fields() {
        let raw = r#"{"number":42,"isDraft":true,"comments":[{"body":"hello"}]}"#;
        assert_eq!(json_u64_field(raw, "number"), Some(42));
        assert_eq!(json_bool_field(raw, "isDraft"), Some(true));
    }

    #[test]
    fn json_escape_strips_only_outer_quotes() {
        let escaped = json_escape("\"");
        let parsed = serde_json::from_str::<String>(&format!("\"{escaped}\"")).unwrap();

        assert_eq!(escaped, "\\\"");
        assert_eq!(parsed, "\"");
    }

    #[test]
    fn json_object_field_reads_nested_object() {
        let raw = r#"{"comments":{"totalCount":3},"other":true}"#;
        let comments = json_object_field(raw, "comments").unwrap();
        assert_eq!(json_u64_field(comments, "totalCount"), Some(3));
    }
}
