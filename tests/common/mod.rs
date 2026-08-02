use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct CompactTempDir {
    pub path: PathBuf,
}

impl CompactTempDir {
    pub fn new(_label: &str) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path =
            PathBuf::from("/tmp").join(format!("pt-{:x}-{unique:x}-{id:x}", std::process::id()));
        fs::create_dir_all(&path).expect("create compact test directory");
        Self { path }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for CompactTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
