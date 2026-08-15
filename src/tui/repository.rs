use std::collections::BTreeMap;
use std::time::Instant;

use crate::config::Config;
use crate::remote::PrSummary;
use crate::repo::Repository;
use crate::session::{Session, WorktreeRepositoryKey, WorktreeSessionKey};

#[derive(Clone, Debug)]
pub(crate) struct ManagedRepo {
    pub repo: Repository,
    pub config: Config,
    pub label: String,
    pub key: Option<char>,
    pub identity: WorktreeRepositoryKey,
    pub pr_summary_poll_in_flight: bool,
    pub pr_summary_last_polled: Option<std::time::Instant>,
    pub pr_summaries: Vec<PrSummary>,
    pub remote_capabilities: Option<crate::remote::Capabilities>,
    pub remote_capability_error: Option<String>,
    pub wt_poll_in_flight: bool,
    pub wt_poll_pending: bool,
    pub wt_last_polled: Option<std::time::Instant>,
    pub wt_last_success: Option<std::time::Instant>,
    pub wt_last_error: Option<String>,
    pub wt_snapshot: Option<crate::worktrunk::WorktrunkSnapshot>,
    pub wt_facts: BTreeMap<WorktreeSessionKey, crate::worktrunk::WorktrunkWorktreeFacts>,
    pub wt_quality: crate::worktrunk::ObservationQuality,
    pub wt_hook_logs: WtHookLogInventory,
    pub default_branch_poll_in_flight: bool,
    pub default_branch_last_polled: Option<std::time::Instant>,
}

#[derive(Clone, Debug)]
pub(crate) struct WtHookLogInventory {
    pub entries: Vec<crate::worktrunk::HookLogEntry>,
    pub quality: crate::worktrunk::ObservationQuality,
    pub last_success: Option<Instant>,
    pub last_error: Option<String>,
    pub refresh_in_flight: bool,
    pub refresh_pending: bool,
}

impl Default for WtHookLogInventory {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            quality: crate::worktrunk::ObservationQuality::NeverLoaded,
            last_success: None,
            last_error: None,
            refresh_in_flight: false,
            refresh_pending: false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SelectedRepoContext {
    pub repo_index: usize,
    pub repo: Repository,
    pub config: Config,
}

#[derive(Clone)]
pub(crate) struct SelectedWorktreeContext {
    pub session_index: usize,
    pub repo: Repository,
    pub config: Config,
}

pub(crate) fn load_worktree_harness_configs(
    repos: &[ManagedRepo],
    sessions: &[Session],
) -> BTreeMap<WorktreeSessionKey, Config> {
    sessions
        .iter()
        .filter_map(|session| {
            let managed = repos.get(session.repo_index)?;
            let association = crate::session::worktree_harness(&managed.repo, session).ok()?;
            let config = managed.config.for_harness(&association.harness_id).ok()?;
            Some((session.identity_key(&managed.identity), config))
        })
        .collect()
}

impl ManagedRepo {
    pub(crate) fn new(repo: Repository, config: Config, key: Option<char>) -> Self {
        let label = crate::workspace::label_for_root(&repo.root);
        Self {
            identity: WorktreeRepositoryKey::new(repo.root.clone()),
            repo,
            config,
            label,
            key,
            pr_summary_poll_in_flight: false,
            pr_summary_last_polled: None,
            pr_summaries: Vec::new(),
            remote_capabilities: None,
            remote_capability_error: None,
            wt_poll_in_flight: false,
            wt_poll_pending: false,
            wt_last_polled: None,
            wt_last_success: None,
            wt_last_error: None,
            wt_snapshot: None,
            wt_facts: BTreeMap::new(),
            wt_quality: crate::worktrunk::ObservationQuality::NeverLoaded,
            wt_hook_logs: WtHookLogInventory::default(),
            default_branch_poll_in_flight: false,
            default_branch_last_polled: None,
        }
    }
}
