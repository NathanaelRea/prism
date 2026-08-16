use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::wait::wait_until;

pub(crate) fn read_events(path: &Path) -> Vec<Value> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    // Adapters append under a file lock, but readers do not take that lock and
    // can observe the final JSON object mid-write. Only parse newline-terminated
    // records; a later polling iteration will see the completed event.
    let complete_len = contents.rfind('\n').map_or(0, |index| index + 1);
    contents[..complete_len]
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("invalid event in {}: {error}: {line}", path.display())
            })
        })
        .collect()
}

pub(crate) fn wait_for_event(
    path: PathBuf,
    timeout: Duration,
    description: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    wait_until(timeout, description, || {
        read_events(&path).into_iter().find(&predicate)
    })
}

pub(crate) fn assert_no_unsupported_events(path: &Path) {
    let unsupported = read_events(path)
        .into_iter()
        .filter(|event| event["unsupported"].as_bool() == Some(true))
        .collect::<Vec<_>>();
    assert!(
        unsupported.is_empty(),
        "strict adapter rejected invocation(s): {unsupported:#?}"
    );
}
