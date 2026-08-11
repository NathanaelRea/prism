pub(crate) mod database;
pub(crate) mod error;
pub(crate) mod observability;
pub(crate) mod pools;
pub(crate) mod remote;
pub(crate) mod remote_coordinator;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod workflow_kernel;
pub(crate) mod workspace;

#[cfg(test)]
mod architecture_tests {
    #[test]
    fn rusqlite_is_not_a_dependency() {
        let manifest = std::fs::read_to_string("Cargo.toml").unwrap();
        let lockfile = std::fs::read_to_string("Cargo.lock").unwrap();
        assert!(!manifest.contains("rusqlite"));
        assert!(!lockfile.contains("name = \"rusqlite\""));
    }
}
