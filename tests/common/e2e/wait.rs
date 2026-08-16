use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn wait_until<T>(
    timeout: Duration,
    description: &str,
    mut predicate: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {description}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
