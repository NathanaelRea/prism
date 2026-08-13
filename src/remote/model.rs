use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use sha2::{Digest as _, Sha256};

use serde::{Deserialize, Serialize};

use super::RemoteError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    GitHub,
    GitLab,
    Forgejo,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::Forgejo => "Forgejo",
        })
    }
}

impl ProviderKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "github" => Some(Self::GitHub),
            "gitlab" => Some(Self::GitLab),
            "forgejo" => Some(Self::Forgejo),
            _ => None,
        }
    }

    pub(crate) fn config_label(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Forgejo => "forgejo",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderItemKind {
    Issue,
    ChangeRequest,
}

/// Canonical provider work-item identity. Issue-shaped API responses retain
/// their actual kind, so a change request can never be admitted as an Issue.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct ProviderItemId {
    repository: RemoteRepositoryId,
    native_id: String,
    kind: ProviderItemKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct IssueId(ProviderItemId);

impl ProviderItemId {
    pub(crate) fn new(
        repository: RemoteRepositoryId,
        native_id: impl Into<String>,
        kind: ProviderItemKind,
    ) -> Result<Self, IdentityError> {
        let native_id = native_id.into();
        if native_id.trim().is_empty() || native_id.chars().any(char::is_control) {
            return Err(IdentityError::new("provider item native ID is invalid"));
        }
        Ok(Self {
            repository,
            native_id,
            kind,
        })
    }

    pub(crate) fn kind(&self) -> ProviderItemKind {
        self.kind
    }
    pub(crate) fn repository(&self) -> &RemoteRepositoryId {
        &self.repository
    }
    pub(crate) fn native_id(&self) -> &str {
        &self.native_id
    }
    pub(crate) fn as_issue(&self) -> Option<IssueId> {
        (self.kind == ProviderItemKind::Issue).then(|| IssueId(self.clone()))
    }

    pub(crate) fn canonical_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.repository.provider.config_label(),
            self.repository.host,
            self.repository.project_path,
            match self.kind {
                ProviderItemKind::Issue => format!("issue:{}", self.native_id),
                ProviderItemKind::ChangeRequest => format!("change_request:{}", self.native_id),
            }
        )
    }
}

impl IssueId {
    pub(crate) fn item(&self) -> &ProviderItemId {
        &self.0
    }
}

/// Complete normalized provider facts consumed by intake. The revision covers
/// all fields, including free-form content and authenticated relationship facts.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderItemObservation {
    pub id: ProviderItemId,
    pub title: String,
    pub body: String,
    pub lifecycle: String,
    pub author: String,
    pub author_relationship: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub assignees: Vec<String>,
    pub updated_at: Option<String>,
}

impl ProviderItemObservation {
    pub(crate) fn revision(&self) -> String {
        let bytes = serde_json::to_vec(&(
            self.id.canonical_key(),
            &self.title,
            &self.body,
            &self.lifecycle,
            &self.author,
            &self.author_relationship,
            &self.labels,
            &self.assignees,
            &self.updated_at,
        ))
        .expect("normalized Provider Item serializes");
        let digest = Sha256::digest(bytes);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Cache state for provider intake. A failed refresh retains the last exact
/// observation as `Stale`; partial responses are never eligible to trigger or
/// authorize work.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub(crate) enum ProviderItemObservationState {
    NeverLoaded,
    Current(ProviderItemObservation),
    Stale(ProviderItemObservation),
    Partial(ProviderItemObservation),
    Failed { safe_error: String },
    ConfirmedAbsent,
}

impl ProviderItemObservationState {
    pub(crate) fn authoritative_present(&self) -> Option<&ProviderItemObservation> {
        match self {
            Self::Current(value) => Some(value),
            Self::NeverLoaded
            | Self::Stale(_)
            | Self::Partial(_)
            | Self::Failed { .. }
            | Self::ConfirmedAbsent => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityError(String);

impl IdentityError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IdentityError {}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct HostIdentity {
    hostname: String,
    port: Option<u16>,
}

impl HostIdentity {
    pub(crate) fn new(hostname: impl AsRef<str>, port: Option<u16>) -> Result<Self, IdentityError> {
        let hostname = hostname.as_ref().trim_end_matches('.').to_ascii_lowercase();
        validate_hostname(&hostname)?;
        if port == Some(0) {
            return Err(IdentityError::new("host port must be greater than zero"));
        }
        Ok(Self { hostname, port })
    }

    pub(crate) fn hostname(&self) -> &str {
        &self.hostname
    }

    pub(crate) fn parse(value: &str) -> Result<Self, IdentityError> {
        match value.rsplit_once(':') {
            Some((hostname, port)) => {
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or_else(|| IdentityError::new("host port is invalid"))?;
                Self::new(hostname, Some(port))
            }
            None => Self::new(value, None),
        }
    }

    pub(crate) fn port(&self) -> Option<u16> {
        self.port
    }

    pub(crate) fn without_port(&self) -> Self {
        Self {
            hostname: self.hostname.clone(),
            port: None,
        }
    }
}

impl fmt::Display for HostIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hostname)?;
        if let Some(port) = self.port {
            write!(formatter, ":{port}")?;
        }
        Ok(())
    }
}

fn validate_hostname(hostname: &str) -> Result<(), IdentityError> {
    if hostname.is_empty() || hostname.len() > 253 || !hostname.is_ascii() {
        return Err(IdentityError::new(
            "host must be a non-empty ASCII DNS name",
        ));
    }
    for label in hostname.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(IdentityError::new("host is not a valid DNS name"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebScheme {
    Http,
    Https,
}

impl WebScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteBase {
    scheme: WebScheme,
    host: HostIdentity,
    path_prefix: String,
}

impl RemoteBase {
    pub(crate) fn new(
        scheme: WebScheme,
        host: HostIdentity,
        path_prefix: impl AsRef<str>,
    ) -> Result<Self, IdentityError> {
        let path_prefix = canonical_path_prefix(path_prefix.as_ref())?;
        Ok(Self {
            scheme,
            host,
            path_prefix,
        })
    }

    pub(crate) fn scheme(&self) -> WebScheme {
        self.scheme
    }

    pub(crate) fn host(&self) -> &HostIdentity {
        &self.host
    }

    pub(crate) fn path_prefix(&self) -> &str {
        &self.path_prefix
    }

    pub(crate) fn parse(value: &str, allow_http: bool) -> Result<Self, IdentityError> {
        let (scheme, remainder) = if let Some(remainder) = value.strip_prefix("https://") {
            (WebScheme::Https, remainder)
        } else if let Some(remainder) = value.strip_prefix("http://") {
            if !allow_http {
                return Err(IdentityError::new(
                    "plain HTTP base URL requires allow_http = true",
                ));
            }
            (WebScheme::Http, remainder)
        } else {
            return Err(IdentityError::new(
                "base URL must use https:// or explicitly allowed http://",
            ));
        };
        if remainder.contains(['?', '#', '@']) || remainder.ends_with('/') {
            return Err(IdentityError::new(
                "base URL must not contain credentials, query, fragment, or trailing slash",
            ));
        }
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        let (hostname, port) = match authority.rsplit_once(':') {
            Some((hostname, port)) => {
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or_else(|| IdentityError::new("base URL port is invalid"))?;
                (hostname, Some(port))
            }
            None => (authority, None),
        };
        let default_port = match scheme {
            WebScheme::Http => 80,
            WebScheme::Https => 443,
        };
        Self::new(
            scheme,
            HostIdentity::new(hostname, port.filter(|port| *port != default_port))?,
            path,
        )
    }
}

impl fmt::Display for RemoteBase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}://{}{}",
            self.scheme.as_str(),
            self.host,
            self.path_prefix
        )
    }
}

fn canonical_path_prefix(path: &str) -> Result<String, IdentityError> {
    if path.contains(['?', '#', '\\']) || path.chars().any(char::is_control) {
        return Err(IdentityError::new("base path contains invalid characters"));
    }
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(String::new());
    }
    if path
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(IdentityError::new("base path contains an invalid segment"));
    }
    Ok(format!("/{path}"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteRepositoryId {
    provider: ProviderKind,
    host: HostIdentity,
    project_path: String,
}

impl RemoteRepositoryId {
    pub(crate) fn new(
        provider: ProviderKind,
        host: HostIdentity,
        project_path: impl AsRef<str>,
    ) -> Result<Self, IdentityError> {
        Ok(Self {
            provider,
            host,
            project_path: canonical_project_path(project_path.as_ref())?,
        })
    }

    pub(crate) fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub(crate) fn host(&self) -> &HostIdentity {
        &self.host
    }

    pub(crate) fn project_path(&self) -> &str {
        &self.project_path
    }

    pub(crate) fn project_path_eq(&self, other: &str) -> bool {
        project_paths_equal(self.provider, &self.project_path, other)
    }
}

impl PartialEq for RemoteRepositoryId {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.host == other.host
            && project_paths_equal(self.provider, &self.project_path, &other.project_path)
    }
}

impl Eq for RemoteRepositoryId {}

impl PartialOrd for RemoteRepositoryId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RemoteRepositoryId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.provider
            .cmp(&other.provider)
            .then_with(|| self.host.cmp(&other.host))
            .then_with(|| {
                project_path_comparison_key(self.provider, &self.project_path).cmp(
                    &project_path_comparison_key(other.provider, &other.project_path),
                )
            })
    }
}

impl Hash for RemoteRepositoryId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.provider.hash(state);
        self.host.hash(state);
        project_path_comparison_key(self.provider, &self.project_path).hash(state);
    }
}

impl fmt::Display for RemoteRepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.host, self.project_path)
    }
}

pub(super) fn canonical_project_path(path: &str) -> Result<String, IdentityError> {
    if path.contains(['?', '#', '\\'])
        || path
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(IdentityError::new(
            "project path contains invalid characters",
        ));
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.split('/').count() < 2
        || path
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(IdentityError::new(
            "project path must contain a namespace and repository",
        ));
    }
    Ok(path.to_string())
}

fn project_paths_equal(provider: ProviderKind, first: &str, second: &str) -> bool {
    match provider {
        ProviderKind::GitHub => first.eq_ignore_ascii_case(second),
        ProviderKind::GitLab | ProviderKind::Forgejo => first == second,
    }
}

fn project_path_comparison_key(provider: ProviderKind, path: &str) -> std::borrow::Cow<'_, str> {
    match provider {
        ProviderKind::GitHub => std::borrow::Cow::Owned(path.to_ascii_lowercase()),
        ProviderKind::GitLab | ProviderKind::Forgejo => std::borrow::Cow::Borrowed(path),
    }
}

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if value.is_empty() || value.chars().any(char::is_control) {
                    return Err(IdentityError::new("native identifier must not be empty"));
                }
                Ok(Self(value))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

opaque_id!(NativeRepositoryId);
opaque_id!(NativeChangeRequestId);
opaque_id!(NativeReviewThreadId);
opaque_id!(NativeMergeGuard);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteRepository {
    pub(crate) id: RemoteRepositoryId,
    pub(crate) native_id: Option<NativeRepositoryId>,
    pub(crate) web_base: RemoteBase,
    pub(crate) api_base: RemoteBase,
}

impl RemoteRepository {
    pub(crate) fn new(
        id: RemoteRepositoryId,
        native_id: Option<NativeRepositoryId>,
        web_base: RemoteBase,
        api_base: RemoteBase,
    ) -> Self {
        Self {
            id,
            native_id,
            web_base,
            api_base,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChangeRequestId {
    repository: RemoteRepositoryId,
    native_id: NativeChangeRequestId,
    display_number: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CanonicalChangeRequestIdentity {
    provider: ProviderKind,
    canonical_host: String,
    project_path: String,
    native_id: String,
    source_provider: ProviderKind,
    source_canonical_host: String,
    source_project_path: String,
    target_provider: ProviderKind,
    target_canonical_host: String,
    target_project_path: String,
}

impl PartialEq for CanonicalChangeRequestIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.canonical_host == other.canonical_host
            && project_paths_equal(self.provider, &self.project_path, &other.project_path)
            && self.native_id == other.native_id
            && self.source_provider == other.source_provider
            && self.source_canonical_host == other.source_canonical_host
            && project_paths_equal(
                self.source_provider,
                &self.source_project_path,
                &other.source_project_path,
            )
            && self.target_provider == other.target_provider
            && self.target_canonical_host == other.target_canonical_host
            && project_paths_equal(
                self.target_provider,
                &self.target_project_path,
                &other.target_project_path,
            )
    }
}

impl Eq for CanonicalChangeRequestIdentity {}

impl CanonicalChangeRequestIdentity {
    pub(crate) fn new(
        repository: &RemoteRepositoryId,
        native_id: &NativeChangeRequestId,
        source_repository: &RemoteRepositoryId,
        target_repository: &RemoteRepositoryId,
    ) -> Self {
        Self {
            provider: repository.provider(),
            canonical_host: repository.host().to_string(),
            project_path: repository.project_path().to_string(),
            native_id: native_id.as_str().to_string(),
            source_provider: source_repository.provider(),
            source_canonical_host: source_repository.host().to_string(),
            source_project_path: source_repository.project_path().to_string(),
            target_provider: target_repository.provider(),
            target_canonical_host: target_repository.host().to_string(),
            target_project_path: target_repository.project_path().to_string(),
        }
    }

    pub(crate) fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub(crate) fn canonical_host(&self) -> &str {
        &self.canonical_host
    }

    pub(crate) fn project_path(&self) -> &str {
        &self.project_path
    }

    pub(crate) fn native_id(&self) -> &str {
        &self.native_id
    }

    pub(crate) fn source_provider(&self) -> ProviderKind {
        self.source_provider
    }

    pub(crate) fn source_canonical_host(&self) -> &str {
        &self.source_canonical_host
    }

    pub(crate) fn source_project_path(&self) -> &str {
        &self.source_project_path
    }

    pub(crate) fn target_provider(&self) -> ProviderKind {
        self.target_provider
    }

    pub(crate) fn target_canonical_host(&self) -> &str {
        &self.target_canonical_host
    }

    pub(crate) fn target_project_path(&self) -> &str {
        &self.target_project_path
    }

    pub(crate) fn change_request_id(
        &self,
        display_number: Option<u64>,
    ) -> Result<ChangeRequestId, IdentityError> {
        Ok(ChangeRequestId::new(
            RemoteRepositoryId::new(
                self.provider,
                HostIdentity::parse(&self.canonical_host)?,
                &self.project_path,
            )?,
            NativeChangeRequestId::new(self.native_id.clone())?,
            display_number,
        ))
    }

    pub(crate) fn source_repository(&self) -> Result<RemoteRepositoryId, IdentityError> {
        RemoteRepositoryId::new(
            self.source_provider,
            HostIdentity::parse(&self.source_canonical_host)?,
            &self.source_project_path,
        )
    }

    pub(crate) fn target_repository(&self) -> Result<RemoteRepositoryId, IdentityError> {
        RemoteRepositoryId::new(
            self.target_provider,
            HostIdentity::parse(&self.target_canonical_host)?,
            &self.target_project_path,
        )
    }

    pub(crate) fn stable_hash(&self) -> u64 {
        let provider = self.provider.to_string();
        let source_provider = self.source_provider.to_string();
        let target_provider = self.target_provider.to_string();
        let mut hash = 0xcbf29ce484222325_u64;
        let project_path = project_path_comparison_key(self.provider, &self.project_path);
        let source_project_path =
            project_path_comparison_key(self.source_provider, &self.source_project_path);
        let target_project_path =
            project_path_comparison_key(self.target_provider, &self.target_project_path);
        for component in [
            provider.as_str(),
            &self.canonical_host,
            project_path.as_ref(),
            &self.native_id,
            source_provider.as_str(),
            &self.source_canonical_host,
            source_project_path.as_ref(),
            target_provider.as_str(),
            &self.target_canonical_host,
            target_project_path.as_ref(),
        ] {
            for byte in component.bytes().chain(std::iter::once(0)) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        hash
    }
}

impl ChangeRequestId {
    pub(crate) fn new(
        repository: RemoteRepositoryId,
        native_id: NativeChangeRequestId,
        display_number: Option<u64>,
    ) -> Self {
        Self {
            repository,
            native_id,
            display_number,
        }
    }

    pub(crate) fn repository(&self) -> &RemoteRepositoryId {
        &self.repository
    }

    pub(crate) fn native_id(&self) -> &NativeChangeRequestId {
        &self.native_id
    }

    pub(crate) fn display_number(&self) -> Option<u64> {
        self.display_number
    }

    pub(crate) fn display_label(&self) -> String {
        match (self.repository.provider, self.display_number) {
            (ProviderKind::GitLab, Some(number)) => format!("!{number}"),
            (_, Some(number)) => format!("#{number}"),
            (_, None) => self.native_id.to_string(),
        }
    }
}

// Display metadata can change without changing the mutation identity.
impl PartialEq for ChangeRequestId {
    fn eq(&self, other: &Self) -> bool {
        self.repository == other.repository && self.native_id == other.native_id
    }
}

impl Eq for ChangeRequestId {}

impl Hash for ChangeRequestId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repository.hash(state);
        self.native_id.hash(state);
    }
}

impl fmt::Display for ChangeRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.repository, self.display_label())
    }
}

macro_rules! normalized_state {
    ($name:ident { $($variant:ident => [$($native:literal),+]),+ $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(crate) enum $name {
            $($variant,)+
            Unknown(String),
        }

        impl $name {
            pub(crate) fn from_native(value: impl Into<String>) -> Self {
                let value = value.into();
                match value.trim().to_ascii_lowercase().as_str() {
                    $($($native)|+ => Self::$variant,)+
                    _ => Self::Unknown(value),
                }
            }
        }
    };
}

normalized_state!(LifecycleState {
    Open => ["open", "opened"],
    Closed => ["closed"],
    Merged => ["merged"]
});

normalized_state!(ReviewDecision {
    Approved => ["approved"],
    ChangesRequested => ["changes_requested", "changes requested", "request_changes"],
    ReviewRequired => ["review_required", "review required"],
    Pending => ["pending", "reviewed", "commented"],
    Dismissed => ["dismissed"]
});

normalized_state!(MergeabilityState {
    Mergeable => ["mergeable", "can_be_merged"],
    Conflicting => ["conflicting", "conflict", "cannot_be_merged"],
    Blocked => ["blocked", "not_approved"],
    Behind => ["behind", "need_rebase"]
});

normalized_state!(CheckState {
    Pending => ["pending", "running", "in_progress", "queued"],
    Passed => ["passed", "success", "successful"],
    Failed => ["failed", "failure", "error"],
    Cancelled => ["cancelled", "canceled"],
    Skipped => ["skipped", "neutral"],
    Mixed => ["mixed"]
});

normalized_state!(QueueState {
    NotQueued => ["not_queued", "none"],
    Queued => ["queued", "pending"],
    Running => ["running", "active"],
    Blocked => ["blocked"],
    Complete => ["complete", "completed", "merged"]
});

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeStateEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) lifecycle: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) review: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) mergeability: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) check: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) queue: Vec<String>,
}

impl NativeStateEvidence {
    pub(crate) fn retain(values: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut retained = Vec::new();
        for value in values {
            if !value.trim().is_empty() && !retained.contains(&value) {
                retained.push(value);
            }
        }
        retained
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Observation<T> {
    #[default]
    NotLoaded,
    Unsupported,
    Unconfigured,
    Unauthorized,
    Known(T),
    AuthoritativelyAbsent,
    EmptyKnown,
    Stale {
        value: T,
        error: Option<RemoteError>,
    },
    Failed(RemoteError),
}

impl<T> Observation<T> {
    pub(crate) fn is_authoritative(&self) -> bool {
        matches!(
            self,
            Self::Known(_) | Self::AuthoritativelyAbsent | Self::EmptyKnown
        )
    }

    pub(crate) fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) | Self::Stale { value, .. } => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadAssociation {
    pub(crate) change_request: ChangeRequestId,
    pub(crate) head_sha: String,
}

impl HeadAssociation {
    pub(crate) fn new(change_request: ChangeRequestId, head_sha: impl Into<String>) -> Self {
        Self {
            change_request,
            head_sha: head_sha.into(),
        }
    }

    pub(crate) fn matches(&self, change_request: &ChangeRequestId, head_sha: &str) -> bool {
        self.change_request == *change_request && self.head_sha == head_sha
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeRequest {
    pub(crate) id: ChangeRequestId,
    pub(crate) source_repository: RemoteRepositoryId,
    pub(crate) target_repository: RemoteRepositoryId,
    pub(crate) source_branch: String,
    pub(crate) target_branch: String,
    pub(crate) head_sha: String,
}

impl ChangeRequest {
    pub(crate) fn head_association(&self) -> HeadAssociation {
        HeadAssociation::new(self.id.clone(), self.head_sha.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeRequestSummary {
    pub(crate) change_request: ChangeRequest,
    pub(crate) title: String,
    pub(crate) author: String,
    pub(crate) body: String,
    pub(crate) web_url: Option<String>,
    pub(crate) lifecycle: LifecycleState,
    pub(crate) review_decision: ReviewDecision,
    pub(crate) requested_reviewers: Vec<String>,
    pub(crate) mergeability: MergeabilityState,
    pub(crate) check_state: CheckState,
    pub(crate) queue_state: QueueState,
    pub(crate) native_state_evidence: NativeStateEvidence,
    pub(crate) comment_count: u64,
    pub(crate) draft: bool,
    pub(crate) updated_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Comment {
    pub(crate) native_id: String,
    pub(crate) author: String,
    pub(crate) body: String,
    pub(crate) created_at: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Review {
    pub(crate) native_id: String,
    pub(crate) author: String,
    pub(crate) decision: ReviewDecision,
    pub(crate) body: String,
    pub(crate) submitted_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewThread {
    pub(crate) native_id: NativeReviewThreadId,
    pub(crate) resolvable: bool,
    pub(crate) resolved: bool,
    pub(crate) comments: Vec<Comment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckContext {
    pub(crate) name: String,
    pub(crate) state: CheckState,
    pub(crate) native_state: String,
    pub(crate) web_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiFailure {
    pub(crate) pipeline: String,
    pub(crate) job: String,
    pub(crate) native_conclusion: String,
    pub(crate) web_url: Option<String>,
    pub(crate) native_run_id: String,
    pub(crate) log_tail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeRequestDetails {
    pub(crate) association: Option<HeadAssociation>,
    pub(crate) comments: Observation<Vec<Comment>>,
    pub(crate) reviews: Observation<Vec<Review>>,
    pub(crate) review_threads: Observation<Vec<ReviewThread>>,
    pub(crate) changed_files: Observation<Vec<String>>,
    pub(crate) checks: Observation<Vec<CheckContext>>,
    pub(crate) ci_failures: Observation<Vec<CiFailure>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PolicyFacts {
    pub(crate) required_checks: Observation<Vec<String>>,
    pub(crate) required_approvals: Observation<u32>,
    pub(crate) conversations_must_be_resolved: Observation<bool>,
    pub(crate) source_must_be_up_to_date: Observation<bool>,
    pub(crate) queue_required: Observation<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepositoryPolicy {
    pub(crate) repository: Option<RemoteRepositoryId>,
    pub(crate) target_branch: String,
    pub(crate) facts: PolicyFacts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FetchChangeRequest {
    pub(crate) id: ChangeRequestId,
    pub(crate) source_repository: RemoteRepositoryId,
    pub(crate) source_branch: String,
    pub(crate) expected_head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateChangeRequest {
    pub(crate) source_repository: RemoteRepositoryId,
    pub(crate) target_repository: RemoteRepositoryId,
    pub(crate) source_branch: String,
    pub(crate) target_branch: String,
    pub(crate) expected_head_sha: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolveReviewThread {
    pub(crate) id: ChangeRequestId,
    pub(crate) thread_id: NativeReviewThreadId,
    pub(crate) expected_head_sha: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewSubmissionKind {
    Approve,
    Comment,
    RequestChanges,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubmitReview {
    pub(crate) id: ChangeRequestId,
    pub(crate) expected_head_sha: String,
    pub(crate) kind: ReviewSubmissionKind,
    pub(crate) body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuardedMerge {
    pub(crate) id: ChangeRequestId,
    pub(crate) target_repository: RemoteRepositoryId,
    pub(crate) target_branch: String,
    pub(crate) expected_source_sha: String,
    pub(crate) method: MergeMethod,
    pub(crate) native_guard: Option<NativeMergeGuard>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeMutationOutcome {
    Merged,
    Pending,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeMutationResult {
    pub(crate) outcome: MergeMutationOutcome,
    pub(crate) native_state: String,
    pub(crate) summary: ChangeRequestSummary,
}

impl MergeMutationResult {
    pub(crate) fn from_summary(
        summary: ChangeRequestSummary,
        native_state: impl Into<String>,
    ) -> Self {
        let outcome = if summary.lifecycle == LifecycleState::Merged {
            MergeMutationOutcome::Merged
        } else if matches!(
            summary.queue_state,
            QueueState::Queued | QueueState::Running | QueueState::Blocked
        ) || summary
            .native_state_evidence
            .queue
            .iter()
            .any(|state| native_queue_evidence_is_positive(state))
        {
            MergeMutationOutcome::Pending
        } else {
            MergeMutationOutcome::Uncertain
        };
        Self {
            outcome,
            native_state: native_state.into(),
            summary,
        }
    }
}

pub(crate) fn native_queue_evidence_is_positive(state: &str) -> bool {
    let state = state.trim().to_ascii_lowercase();
    if let Some((key, value)) = state.split_once('=') {
        return matches!(
            (key.trim(), value.trim()),
            (
                "merge_when_pipeline_succeeds" | "auto_merge_enabled",
                "true"
            )
        );
    }
    matches!(
        state.as_str(),
        "queued"
            | "pending"
            | "running"
            | "active"
            | "blocked"
            | "awaiting_checks"
            | "awaiting_merge"
            | "preparing"
            | "merge_train"
    )
}

impl GuardedMerge {
    pub(crate) fn validate_observation(
        &self,
        summary: &ChangeRequestSummary,
    ) -> Result<(), IdentityError> {
        let observed = &summary.change_request;
        if observed.id != self.id
            || observed.target_repository != self.target_repository
            || observed.target_branch != self.target_branch
        {
            return Err(IdentityError::new(
                "change request target changed since merge authorization",
            ));
        }
        if observed.head_sha != self.expected_source_sha {
            return Err(IdentityError::new(
                "change request head changed since merge authorization",
            ));
        }
        if summary.lifecycle != LifecycleState::Open {
            return Err(IdentityError::new(
                "change request lifecycle is not authoritatively open",
            ));
        }
        Ok(())
    }
}
