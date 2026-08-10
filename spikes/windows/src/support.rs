use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub type SpikeResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct SpikeError(String);

impl Display for SpikeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SpikeError {}

pub fn fail<T>(message: impl Into<String>) -> SpikeResult<T> {
    Err(Box::new(SpikeError(message.into())))
}

pub fn require(condition: bool, message: impl Into<String>) -> SpikeResult {
    if condition { Ok(()) } else { fail(message) }
}

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn unique_name(prefix: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{timestamp:x}-{counter:x}", std::process::id())
}

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> SpikeResult<Self> {
        let path = std::env::temp_dir().join(unique_name(prefix));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..10 {
            match fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        eprintln!(
            "warning: could not remove spike directory {}",
            self.0.display()
        );
    }
}
