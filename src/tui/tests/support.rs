use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::AgentState;
use crate::config::Config;
use crate::remote::{PrCache, PrSummary};
use crate::repo::Repository;
use crate::session::Session;

use super::super::{ManagedRepo, Tui};

pub(super) fn test_tui() -> Tui {
    let repos = vec![
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-one"),
            },
            test_config(),
            Some('1'),
        ),
        ManagedRepo::new(
            Repository {
                root: PathBuf::from("/repo-two"),
            },
            test_config(),
            Some('2'),
        ),
    ];
    let sessions = vec![
        test_session(0, "/repo-one", "main"),
        test_session(0, "/repo-one", "feature-one"),
        test_session(1, "/repo-two", "main"),
        test_session(1, "/repo-two", "feature-two"),
    ];
    Tui::new(repos, 0, sessions)
}

pub(super) fn test_session(repo_index: usize, root: &str, branch: &str) -> Session {
    let path = PathBuf::from(format!("{root}/{branch}"));
    let _ = fs::create_dir_all(&path);
    Session {
        repo_index,
        repo_label: format!("repo-{repo_index}"),
        repo_key: None,
        path: path.clone(),
        incarnation: String::new(),
        path_display: path.display().to_string(),
        branch: branch.to_string(),
        prompt_summary: String::new(),
        classification: crate::session::SessionClassification::Work,
        visibility: 0,
        adopted: false,
        hidden: false,
        status_label: "clean".to_string(),
        agent_state: AgentState::Idle,
        opencode_status: None,
        pr: PrCache::default(),
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

pub(super) fn test_config() -> Config {
    let mut config = crate::test_support::test_config();
    config.default_agent = "opencode".to_string();
    config.default_base = Some("main".to_string());
    config
}

pub(super) fn test_pr_summary(merged: bool) -> PrSummary {
    PrSummary {
        number: 1,
        change_request_identity: None,
        native_state_evidence: crate::remote::NativeStateEvidence::default(),
        title: "PR".to_string(),
        author: "author".to_string(),
        body: String::new(),
        url: "https://example.test/pr/1".to_string(),
        state: if merged { "MERGED" } else { "OPEN" }.to_string(),
        review_decision: String::new(),
        requested_reviewers: Vec::new(),
        head_ref: "feature".to_string(),
        base_ref: "main".to_string(),
        head_sha: "abc123".to_string(),
        updated_at: String::new(),
        check_status: String::new(),
        merge_state_status: String::new(),
        queue_state: String::new(),
        comment_count: 0,
        merged,
        draft: false,
    }
}

pub(super) fn test_change_request_identity(
    provider: crate::remote::ProviderKind,
) -> crate::remote::CanonicalChangeRequestIdentity {
    test_change_request_identity_for(provider, "example/repo", "change-request-1")
}

pub(super) fn test_change_request_identity_for(
    provider: crate::remote::ProviderKind,
    project_path: &str,
    native_id: &str,
) -> crate::remote::CanonicalChangeRequestIdentity {
    let host = match provider {
        crate::remote::ProviderKind::GitHub => "github.com",
        crate::remote::ProviderKind::GitLab => "gitlab.com",
        crate::remote::ProviderKind::Forgejo => "codeberg.org",
    };
    let host = crate::remote::HostIdentity::new(host, None).unwrap();
    let repository = crate::remote::RemoteRepositoryId::new(provider, host, project_path).unwrap();
    crate::remote::CanonicalChangeRequestIdentity::new(
        &repository,
        &crate::remote::NativeChangeRequestId::new(native_id).unwrap(),
        &repository,
        &repository,
    )
}

pub(super) fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
}
