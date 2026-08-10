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

#[cfg(test)]
pub fn truncate_line(text: &str, max_chars: usize) -> String {
    truncate(&single_line(text), max_chars)
}

pub fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii_control() { ' ' } else { ch })
        .collect()
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for seq_ch in chars.by_ref() {
                if seq_ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn status_count(status: &str, key: &str) -> Option<usize> {
    let mut words = status.split_whitespace();
    while let Some(word) = words.next() {
        if word == key {
            return words.next()?.parse().ok();
        }
    }
    None
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = env::var_os("HOME");
    home.map(PathBuf::from)
}

pub fn prism_config_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = env::var_os("APPDATA") {
        return PathBuf::from(path).join("Prism");
    }
    #[cfg(not(windows))]
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("prism");
    }
    if let Some(home) = home_dir() {
        #[cfg(windows)]
        return home.join("AppData/Roaming/Prism");
        #[cfg(not(windows))]
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
    let safe = branch
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_ascii_control() => '_',
            ch => ch,
        })
        .collect::<String>();
    windows_safe_component_if_needed(safe, "branch")
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
    let safe = if safe.is_empty() {
        "repo".to_string()
    } else {
        safe
    };
    windows_safe_component_if_needed(safe, "repo")
}

#[cfg(not(windows))]
fn windows_safe_component_if_needed(value: String, _fallback: &str) -> String {
    value
}

#[cfg(windows)]
fn windows_safe_component_if_needed(value: String, fallback: &str) -> String {
    windows_safe_component(&value, fallback)
}

#[cfg(any(windows, test))]
fn windows_safe_component(value: &str, fallback: &str) -> String {
    let trimmed = value.trim_end_matches([' ', '.']);
    let mut safe = if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    };
    let stem = safe
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    if reserved {
        safe.insert(0, '_');
    }
    safe
}

#[cfg(unix)]
pub fn timestamp_label() -> String {
    let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return "now".to_string();
    };
    let Ok(timestamp) = libc::time_t::try_from(elapsed.as_secs()) else {
        return "now".to_string();
    };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes `local` when it returns a non-null pointer.
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return "now".to_string();
    }
    // SAFETY: the successful localtime_r call above initialized `local`.
    let local = unsafe { local.assume_init() };
    format!(
        "{:02}:{:02}:{:02}",
        local.tm_hour, local.tm_min, local.tm_sec
    )
}

#[cfg(windows)]
pub fn timestamp_label() -> String {
    // SAFETY: GetLocalTime returns an initialized SYSTEMTIME value.
    let local = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}",
        local.wHour, local.wMinute, local.wSecond
    )
}

#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        safe_path_component, single_line, stable_hash, status_count, strip_ansi, truncate_line,
        windows_safe_component,
    };

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
    fn strip_ansi_removes_style_sequences() {
        assert_eq!(strip_ansi("\x1b[31m•\x1b[0m dirty"), "• dirty");
    }

    #[test]
    fn status_count_reads_status_label_counts() {
        assert_eq!(status_count("dirty 2 ahead 3 behind 1", "dirty"), Some(2));
        assert_eq!(status_count("dirty 2 ahead 3 behind 1", "ahead"), Some(3));
        assert_eq!(status_count("clean", "dirty"), None);
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

    #[test]
    fn windows_components_avoid_reserved_names_and_trailing_dots() {
        assert_eq!(windows_safe_component("CON", "repo"), "_CON");
        assert_eq!(windows_safe_component("lpt9.log", "repo"), "_lpt9.log");
        assert_eq!(windows_safe_component("feature. ", "repo"), "feature");
        assert_eq!(windows_safe_component("...", "repo"), "repo");
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_unicode_paths_round_trip_without_ansi_conversion() {
        let root = std::env::temp_dir().join(format!(
            "prism windows path 雪 {} {}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        let long = root
            .join("a".repeat(96))
            .join("b".repeat(96))
            .join("c".repeat(96));
        std::fs::create_dir_all(&long).unwrap();
        let path = long.join("state 雪.txt");
        std::fs::write(&path, "unicode 雪\n").unwrap();
        assert!(path.as_os_str().len() > 260);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "unicode 雪\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
