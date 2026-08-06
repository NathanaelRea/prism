mod adoption;

pub(crate) mod approvals;
pub(crate) mod artifacts;
pub(crate) mod auto_flow;
pub(crate) mod control_plane;
pub(crate) mod database;
pub(crate) mod effects;
pub(crate) mod error;
pub(crate) mod import;
pub(crate) mod notification;
pub(crate) mod observability;
pub(crate) mod plan_run;
pub(crate) mod pools;
pub(crate) mod remote;
pub(crate) mod run_ledger;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod wakeups;
pub(crate) mod workflow;
pub(crate) mod workspace;

#[cfg(test)]
mod architecture_tests {
    use std::fs;
    use std::path::Path;

    fn production_source(path: &Path) -> String {
        let source = fs::read_to_string(path).unwrap();
        source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(&source)
            .to_string()
    }

    fn visit_rust(path: &Path, inspect: &mut impl FnMut(&Path, &str)) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit_rust(&path, inspect);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                inspect(&path, &production_source(&path));
            }
        }
    }

    #[test]
    fn production_persistence_does_not_construct_tokio_runtimes() {
        visit_rust(Path::new("src/persistence"), &mut |path, production| {
            if path.file_name().and_then(|name| name.to_str()) == Some("cli_test_support.rs") {
                return;
            }
            assert!(
                !production.contains("tokio::runtime::Builder")
                    && !production.contains("tokio::runtime::Runtime")
                    && !production.contains("Runtime::new("),
                "{} constructs a Tokio runtime inside persistence",
                path.display()
            );
        });
    }

    #[test]
    fn generalized_persistence_has_no_synchronous_runtime_wrapper() {
        for file in [
            "adoption.rs",
            "approvals.rs",
            "artifacts.rs",
            "control_plane.rs",
            "effects.rs",
            "import.rs",
            "pools.rs",
            "run_ledger.rs",
            "wakeups.rs",
            "workspace.rs",
        ] {
            let path = Path::new("src/persistence").join(file);
            let production = production_source(&path);
            assert!(
                !production.contains("block_on("),
                "{} blocks inside the async workflow persistence model",
                path.display()
            );
        }
    }

    #[test]
    fn production_sql_is_private_to_persistence() {
        visit_rust(Path::new("src"), &mut |path, production| {
            if path.starts_with("src/persistence") {
                return;
            }
            assert!(
                !production.contains("sqlx::") && !production.contains("rusqlite"),
                "{} bypasses a persistence domain interface",
                path.display()
            );
        });
    }

    #[test]
    fn execution_envelopes_do_not_own_database_connections() {
        for file in ["src/workflow/engine.rs", "src/workflow/execution.rs"] {
            let path = Path::new(file);
            let production = production_source(path);
            for forbidden in ["SqliteConnection", "PoolConnection<", "SqlitePool"] {
                assert!(
                    !production.contains(forbidden),
                    "{} stores or accesses a database connection in an execution envelope",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn rusqlite_is_not_a_dependency() {
        let manifest = fs::read_to_string("Cargo.toml").unwrap();
        let lockfile = fs::read_to_string("Cargo.lock").unwrap();
        assert!(!manifest.contains("rusqlite"));
        assert!(!lockfile.contains("name = \"rusqlite\""));
    }

    #[test]
    fn legacy_claim_guards_and_scheduler_are_absent() {
        let persistence = production_source(Path::new("src/persistence/workflow.rs"));
        let worker = production_source(Path::new("src/workflow/worker.rs"));
        for forbidden in [["Claim", "Session"].concat(), ["claim", "_guard_"].concat()] {
            assert!(
                !persistence.contains(&forbidden),
                "legacy claim guard remains in repository persistence: {forbidden}"
            );
        }
        assert!(
            !worker.contains(&["schedule", "_queued"].concat()),
            "legacy repository scheduler remains in the worker"
        );
    }

    #[test]
    fn repository_workspace_persistence_has_no_legacy_workflow_authority() {
        let production = production_source(Path::new("src/persistence/workspace.rs"));
        for forbidden in [
            "WorkflowRow",
            "ControlInput",
            "apply_control",
            "linked_plan_owners",
        ] {
            assert!(
                !production.contains(forbidden),
                "repository workspace persistence still exposes legacy workflow state: {forbidden}"
            );
        }
    }

    #[test]
    fn transaction_control_is_not_stored_as_domain_sql() {
        fn inspect(path: &Path) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    inspect(&path);
                    continue;
                }
                let sql = fs::read_to_string(&path).unwrap();
                let normalized = sql
                    .split_whitespace()
                    .collect::<String>()
                    .trim_end_matches(';')
                    .to_ascii_lowercase();
                assert!(
                    !matches!(
                        normalized.as_str(),
                        "begin" | "beginimmediate" | "beginexclusive" | "commit" | "rollback"
                    ),
                    "transaction token SQL must remain in the database implementation: {}",
                    path.display()
                );
            }
        }
        inspect(Path::new("sql"));
    }
}
