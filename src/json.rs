pub fn json_escape(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            other => output.push(other),
        }
    }
    output
}

pub fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)?;
    let rest = &text[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if !value.starts_with('"') {
        return None;
    }
    let mut output = String::new();
    let mut escaped = false;
    for ch in value[1..].chars() {
        if escaped {
            output.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '"' => '"',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(output);
        } else {
            output.push(ch);
        }
    }
    None
}

pub fn json_u64_field(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)?;
    let rest = &text[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

pub fn json_bool_field(text: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)?;
    let rest = &text[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
pub fn json_array_object_count(text: &str, key: &str) -> Option<usize> {
    let array = json_array_field(text, key)?;
    Some(array.matches('{').count())
}

pub fn json_array_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)?;
    let rest = &text[start + needle.len()..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim_start();
    if !value.starts_with('[') {
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
            '[' => depth += 1,
            ']' => {
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

pub fn json_objects_in_array<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    json_array_field(text, key)
        .map(json_top_level_objects)
        .unwrap_or_default()
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

pub fn json_login_field(text: &str) -> Option<String> {
    json_string_field(text, "login")
        .or_else(|| json_string_field(text, "author"))
        .or_else(|| json_string_field(text, "user"))
}

pub fn collect_json_string_fields(text: &str, key: &str, limit: usize) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut values = Vec::new();
    let mut offset = 0;
    while values.len() < limit {
        let Some(relative) = text[offset..].find(&needle) else {
            break;
        };
        let start = offset + relative;
        if let Some(value) = json_string_field(&text[start..], key)
            && !value.trim().is_empty()
        {
            values.push(value);
        }
        offset = start + needle.len();
    }
    values
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
        assert_eq!(json_array_object_count(raw, "comments"), Some(1));
    }

    #[test]
    fn json_object_field_reads_nested_object() {
        let raw = r#"{"comments":{"totalCount":3},"other":true}"#;
        let comments = json_object_field(raw, "comments").unwrap();
        assert_eq!(json_u64_field(comments, "totalCount"), Some(3));
    }
}
