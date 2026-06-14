use std::env;
use std::path::{Path, PathBuf};

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return "~".to_string();
    }
    let mut out = text.chars().take(max_chars - 1).collect::<String>();
    out.push('~');
    out
}

pub fn truncate_line(text: &str, max_chars: usize) -> String {
    truncate(&single_line(text), max_chars)
}

pub fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii_control() { ' ' } else { ch })
        .collect()
}

pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

pub fn prism_config_dir() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("prism");
    }
    if let Some(home) = home_dir() {
        return home.join(".config/prism");
    }
    env::temp_dir().join("prism")
}

pub fn stable_hash(path: &Path) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.display().to_string().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn safe_branch_filename(branch: &str) -> String {
    branch
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_ascii_control() => '_',
            ch => ch,
        })
        .collect()
}

pub fn safe_path_component(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "repo".to_string()
    } else {
        safe
    }
}

pub fn timestamp_label() -> String {
    match crate::process::run_capture(std::process::Command::new("date").arg("+%H:%M:%S")) {
        Ok(value) => value.trim().to_string(),
        Err(_) => "now".to_string(),
    }
}

pub fn timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

pub fn yes(value: &str) -> bool {
    matches!(value.trim(), "y" | "Y" | "yes" | "YES")
}

pub fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value.trim()
    }
}

pub fn indent_markdown_block(value: &str) -> String {
    value
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{safe_path_component, single_line, stable_hash, truncate_line};

    #[test]
    fn single_line_replaces_control_characters() {
        assert_eq!(
            single_line("one\ntwo\r\tthree\x1b[31m"),
            "one two  three [31m"
        );
    }

    #[test]
    fn truncate_line_sanitizes_before_truncating() {
        assert_eq!(truncate_line("abc\ndef", 6), "abc d~");
    }

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(
            stable_hash(Path::new("/repo/my project")),
            stable_hash(Path::new("/repo/my project"))
        );
    }

    #[test]
    fn path_component_is_filesystem_safe() {
        assert_eq!(safe_path_component("my project/foo"), "my_project_foo");
        assert_eq!(safe_path_component(""), "repo");
    }
}
