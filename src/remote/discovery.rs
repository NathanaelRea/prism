use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::process::Command;

use super::{
    HostIdentity, IdentityError, ProviderKind, RemoteBase, RemoteRepository, RemoteRepositoryId,
    WebScheme,
};
use crate::config::Config;
use crate::process::{ProcessPolicy, run_capture};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitTransport {
    Https,
    Http,
    Ssh,
}

impl GitTransport {
    fn default_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Http => 80,
            Self::Ssh => 22,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedGitRemote {
    pub(crate) transport: GitTransport,
    pub(crate) host: HostIdentity,
    pub(crate) project_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    Malformed(String),
    InsecureHttp(HostIdentity),
    UnknownHost(HostIdentity),
    ConflictingProfile(HostIdentity),
    Git(String),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => write!(formatter, "invalid Git remote: {message}"),
            Self::InsecureHttp(host) => {
                write!(
                    formatter,
                    "plain HTTP is not allowed for remote host {host}"
                )
            }
            Self::UnknownHost(host) => write!(
                formatter,
                "remote host {host} is not configured; add an explicit remote host profile"
            ),
            Self::ConflictingProfile(host) => {
                write!(
                    formatter,
                    "remote host {host} has conflicting provider profiles"
                )
            }
            Self::Git(message) => formatter.write_str(message),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteUrlKind {
    Fetch,
    Push,
}

pub(crate) fn discover_git_remote(
    path: &Path,
    config: &Config,
    remote_name: &str,
    kind: RemoteUrlKind,
) -> Result<DiscoveredRemote, DiscoveryError> {
    let mut command = Command::new(config.tool("git"));
    command.arg("-C").arg(path).args(["remote", "get-url"]);
    if kind == RemoteUrlKind::Push {
        command.arg("--push");
    }
    command.arg(remote_name);
    let remote = run_capture(&mut command, ProcessPolicy::Metadata)
        .map_err(|error| DiscoveryError::Git(format!("read {remote_name} remote URL: {error}")))?;
    config
        .remote_discovery()
        .map_err(DiscoveryError::Git)?
        .discover(remote.trim())
}

impl std::error::Error for DiscoveryError {}

impl From<IdentityError> for DiscoveryError {
    fn from(error: IdentityError) -> Self {
        Self::Malformed(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GitRemoteParser {
    allow_http: bool,
}

impl GitRemoteParser {
    pub(crate) fn new(allow_http: bool) -> Self {
        Self { allow_http }
    }

    pub(crate) fn parse(&self, remote: &str) -> Result<ParsedGitRemote, DiscoveryError> {
        let parsed = parse_remote(remote)?;
        if parsed.transport == GitTransport::Http && !self.allow_http {
            return Err(DiscoveryError::InsecureHttp(parsed.host));
        }
        Ok(parsed)
    }
}

fn parse_remote(remote: &str) -> Result<ParsedGitRemote, DiscoveryError> {
    if remote.is_empty() || remote.trim() != remote {
        return Err(DiscoveryError::Malformed(
            "remote must be non-empty and have no surrounding whitespace".to_string(),
        ));
    }
    if remote.contains(['?', '#']) || remote.chars().any(char::is_control) {
        return Err(DiscoveryError::Malformed(
            "query strings, fragments, and control characters are not supported".to_string(),
        ));
    }

    if let Some((scheme, remainder)) = remote.split_once("://") {
        let transport = match scheme.to_ascii_lowercase().as_str() {
            "https" => GitTransport::Https,
            "http" => GitTransport::Http,
            "ssh" => GitTransport::Ssh,
            _ => {
                return Err(DiscoveryError::Malformed(format!(
                    "unsupported remote scheme {scheme}"
                )));
            }
        };
        return parse_url_remote(transport, remainder);
    }

    parse_scp_remote(remote)
}

fn parse_url_remote(
    transport: GitTransport,
    remainder: &str,
) -> Result<ParsedGitRemote, DiscoveryError> {
    let (authority, path) = remainder
        .split_once('/')
        .ok_or_else(|| DiscoveryError::Malformed("remote URL has no project path".to_string()))?;
    if authority.is_empty() {
        return Err(DiscoveryError::Malformed(
            "remote URL has no host".to_string(),
        ));
    }

    let authority = match authority.rsplit_once('@') {
        Some((user, authority)) if transport == GitTransport::Ssh && !user.is_empty() => authority,
        Some(_) => {
            return Err(DiscoveryError::Malformed(
                "credentials are not accepted in remote URLs".to_string(),
            ));
        }
        None => authority,
    };
    let (hostname, port) = parse_authority(authority)?;
    let port = port.filter(|port| *port != transport.default_port());
    Ok(ParsedGitRemote {
        transport,
        host: HostIdentity::new(hostname, port)?,
        project_path: super::model::canonical_project_path(path)?,
    })
}

fn parse_authority(authority: &str) -> Result<(&str, Option<u16>), DiscoveryError> {
    if authority.contains(['[', ']']) {
        return Err(DiscoveryError::Malformed(
            "only DNS hostnames are supported".to_string(),
        ));
    }
    let Some((hostname, port)) = authority.rsplit_once(':') else {
        return Ok((authority, None));
    };
    if hostname.contains(':') || hostname.is_empty() || port.is_empty() {
        return Err(DiscoveryError::Malformed(
            "remote host or port is malformed".to_string(),
        ));
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| DiscoveryError::Malformed("remote port is invalid".to_string()))?;
    Ok((hostname, Some(port)))
}

fn parse_scp_remote(remote: &str) -> Result<ParsedGitRemote, DiscoveryError> {
    let (authority, path) = remote.split_once(':').ok_or_else(|| {
        DiscoveryError::Malformed("remote is neither a URL nor SCP-like SSH".to_string())
    })?;
    if authority.contains('/') || authority.is_empty() {
        return Err(DiscoveryError::Malformed(
            "SCP-like remote has an invalid host".to_string(),
        ));
    }
    let hostname = match authority.rsplit_once('@') {
        Some((user, hostname)) if !user.is_empty() && !hostname.is_empty() => hostname,
        Some(_) => {
            return Err(DiscoveryError::Malformed(
                "SCP-like remote has an invalid user or host".to_string(),
            ));
        }
        None => authority,
    };
    Ok(ParsedGitRemote {
        transport: GitTransport::Ssh,
        host: HostIdentity::new(hostname, None)?,
        project_path: super::model::canonical_project_path(path)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostProfile {
    pub(crate) host: HostIdentity,
    pub(crate) provider: ProviderKind,
    pub(crate) allow_http: bool,
    pub(crate) web_base: RemoteBase,
    pub(crate) api_base: RemoteBase,
    pub(crate) credential_environment: Option<String>,
}

impl HostProfile {
    pub(crate) fn new(host: HostIdentity, provider: ProviderKind) -> Result<Self, IdentityError> {
        let web_base = RemoteBase::new(WebScheme::Https, host.clone(), "")?;
        let api_path = match provider {
            ProviderKind::GitHub => "/api/v3",
            ProviderKind::GitLab => "/api/v4",
            ProviderKind::Forgejo => "/api/v1",
        };
        let api_base = RemoteBase::new(WebScheme::Https, host.clone(), api_path)?;
        Ok(Self {
            host,
            provider,
            allow_http: false,
            web_base,
            api_base,
            credential_environment: None,
        })
    }

    pub(crate) fn with_http_allowed(mut self, allow_http: bool) -> Self {
        self.allow_http = allow_http;
        self
    }

    pub(crate) fn with_bases(mut self, web_base: RemoteBase, api_base: RemoteBase) -> Self {
        self.web_base = web_base;
        self.api_base = api_base;
        self
    }

    pub(crate) fn with_credential_environment(
        mut self,
        name: impl Into<String>,
    ) -> Result<Self, IdentityError> {
        let name = name.into();
        let mut characters = name.chars();
        if !characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(IdentityError::new("credential environment name is invalid"));
        }
        self.credential_environment = Some(name);
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveredRemote {
    pub(crate) parsed: ParsedGitRemote,
    pub(crate) repository: RemoteRepository,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteDiscovery {
    profiles: BTreeMap<HostIdentity, HostProfile>,
}

impl RemoteDiscovery {
    pub(crate) fn new(
        explicit_profiles: impl IntoIterator<Item = HostProfile>,
    ) -> Result<Self, DiscoveryError> {
        let mut profiles = builtin_profiles();
        let mut explicit_hosts = BTreeSet::new();
        for profile in explicit_profiles {
            if !explicit_hosts.insert(profile.host.clone()) {
                return Err(DiscoveryError::ConflictingProfile(profile.host));
            }
            if profiles
                .get(&profile.host)
                .is_some_and(|existing| existing.provider != profile.provider)
            {
                return Err(DiscoveryError::ConflictingProfile(profile.host));
            }
            profiles.insert(profile.host.clone(), profile);
        }
        Ok(Self { profiles })
    }

    pub(crate) fn discover(&self, remote: &str) -> Result<DiscoveredRemote, DiscoveryError> {
        // Parse HTTP structurally first; the selected host profile owns the opt-in decision.
        let parsed = GitRemoteParser::new(true).parse(remote)?;
        let profile = self
            .profiles
            .get(&parsed.host)
            .or_else(|| self.profiles.get(&parsed.host.without_port()))
            .ok_or_else(|| DiscoveryError::UnknownHost(parsed.host.clone()))?;
        if parsed.transport == GitTransport::Http && !profile.allow_http {
            return Err(DiscoveryError::InsecureHttp(parsed.host));
        }

        // A Git transport port is not part of the hosting-service identity. API and web
        // operations use the explicitly selected profile host and bases.
        let id =
            RemoteRepositoryId::new(profile.provider, profile.host.clone(), &parsed.project_path)?;
        Ok(DiscoveredRemote {
            parsed,
            repository: RemoteRepository::new(
                id,
                None,
                profile.web_base.clone(),
                profile.api_base.clone(),
            ),
        })
    }

    pub(crate) fn profile(&self, host: &HostIdentity) -> Option<&HostProfile> {
        self.profiles
            .get(host)
            .or_else(|| self.profiles.get(&host.without_port()))
    }
}

impl Default for RemoteDiscovery {
    fn default() -> Self {
        Self::new([]).expect("built-in remote host profiles are valid")
    }
}

fn builtin_profiles() -> BTreeMap<HostIdentity, HostProfile> {
    [github_profile(), gitlab_profile(), codeberg_profile()]
        .into_iter()
        .map(|profile| (profile.host.clone(), profile))
        .collect()
}

fn github_profile() -> HostProfile {
    let host = HostIdentity::new("github.com", None).expect("built-in host is valid");
    HostProfile::new(host.clone(), ProviderKind::GitHub)
        .expect("built-in profile is valid")
        .with_bases(
            RemoteBase::new(WebScheme::Https, host, "").expect("built-in base is valid"),
            RemoteBase::new(
                WebScheme::Https,
                HostIdentity::new("api.github.com", None).expect("built-in API host is valid"),
                "",
            )
            .expect("built-in API base is valid"),
        )
}

fn gitlab_profile() -> HostProfile {
    let host = HostIdentity::new("gitlab.com", None).expect("built-in host is valid");
    HostProfile::new(host, ProviderKind::GitLab).expect("built-in profile is valid")
}

fn codeberg_profile() -> HostProfile {
    let host = HostIdentity::new("codeberg.org", None).expect("built-in host is valid");
    HostProfile::new(host, ProviderKind::Forgejo).expect("built-in profile is valid")
}
