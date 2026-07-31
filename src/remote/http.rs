use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use super::{
    HostProfile, ProviderKind, RemoteError, RemoteErrorClass, RemoteOperation, RetryHint,
    Retryability,
};

const MAX_HEADERS_SIZE: usize = 64 * 1024;
const MAX_PAGES: usize = 100;

pub(super) struct HttpClient {
    api_base: Url,
    agent: ureq::Agent,
    credential_environment: Option<String>,
    response_limit: usize,
    cancelled: Arc<AtomicBool>,
}

pub(super) struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpClient {
    pub(super) fn new(
        profile: &HostProfile,
        timeout: Duration,
        response_limit: usize,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, RemoteError> {
        let api_base = Url::parse(&format!("{}/", profile.api_base))
            .map_err(|_| configuration_error("Forgejo API base URL is invalid"))?;
        if api_base.cannot_be_a_base()
            || !matches!(api_base.scheme(), "http" | "https")
            || !api_base.username().is_empty()
            || api_base.password().is_some()
            || api_base.query().is_some()
            || api_base.fragment().is_some()
        {
            return Err(configuration_error("Forgejo API base URL is invalid"));
        }
        if api_base.scheme() == "http" && !profile.allow_http {
            return Err(configuration_error(
                "plain HTTP is not enabled for this Forgejo host profile",
            ));
        }
        if response_limit == 0 {
            return Err(configuration_error(
                "Forgejo response limit must be greater than zero",
            ));
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .max_response_header_size(MAX_HEADERS_SIZE)
            .timeout_global(Some(timeout))
            .build()
            .into();

        Ok(Self {
            api_base,
            agent,
            credential_environment: profile.credential_environment.clone(),
            response_limit,
            cancelled,
        })
    }

    pub(super) fn get_json<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, RemoteError> {
        let url = self.endpoint(path, query, operation)?;
        let response = self.send(operation, url, None)?;
        response.json(operation)
    }

    pub(super) fn get_json_pages<T: DeserializeOwned>(
        &self,
        operation: RemoteOperation,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<T>, RemoteError> {
        let mut url = self.endpoint(path, query, operation)?;
        let mut values = Vec::new();
        for _ in 0..MAX_PAGES {
            let response = self.send(operation, url.clone(), None)?;
            let next = response.next_link(&self.api_base, operation)?;
            let page = response.json::<Vec<T>>(operation)?;
            let full_page_without_link = next.is_none()
                && requested_page_size(&url).is_some_and(|limit| page.len() >= limit);
            values.extend(page);
            if let Some(next) = next {
                url = next;
                continue;
            }
            if full_page_without_link {
                url = next_numbered_page(&url).ok_or_else(|| pagination_error(operation))?;
            } else {
                return Ok(values);
            }
        }
        Err(RemoteError::new(
            ProviderKind::Forgejo,
            operation,
            RemoteErrorClass::InvalidResponse,
            Retryability::NotRetryable,
            "Forgejo pagination exceeded the page limit",
        ))
    }

    pub(super) fn get_bytes(
        &self,
        operation: RemoteOperation,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<u8>, RemoteError> {
        let url = self.endpoint(path, query, operation)?;
        Ok(self.send(operation, url, None)?.body)
    }

    pub(super) fn send_json<B: Serialize>(
        &self,
        operation: RemoteOperation,
        path: &str,
        body: &B,
    ) -> Result<HttpResponse, RemoteError> {
        let body = serde_json::to_vec(body).map_err(|_| {
            RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "could not encode Forgejo request",
            )
        })?;
        let url = self.endpoint(path, &[], operation)?;
        self.send(operation, url, Some(body))
    }

    fn endpoint(
        &self,
        path: &str,
        query: &[(&str, String)],
        operation: RemoteOperation,
    ) -> Result<Url, RemoteError> {
        if path.starts_with('/') {
            return Err(RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "Forgejo endpoint path must be relative",
            ));
        }
        let mut url = self.api_base.join(path).map_err(|_| {
            RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "Forgejo endpoint path is invalid",
            )
        })?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query {
                pairs.append_pair(name, value);
            }
        }
        Ok(url)
    }

    fn send(
        &self,
        operation: RemoteOperation,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, RemoteError> {
        self.check_cancelled(operation)?;
        if !same_origin(&self.api_base, &url) {
            return Err(RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "Forgejo request was rejected because its origin changed",
            ));
        }

        let authorization = if let Some(environment) = &self.credential_environment {
            let token = std::env::var(environment).map_err(|_| {
                RemoteError::new(
                    ProviderKind::Forgejo,
                    operation,
                    RemoteErrorClass::Authentication,
                    Retryability::NotRetryable,
                    "the configured Forgejo credential environment variable is unavailable",
                )
                .with_retry_hint(RetryHint::Reauthenticate)
            })?;
            if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(RemoteError::new(
                    ProviderKind::Forgejo,
                    operation,
                    RemoteErrorClass::Authentication,
                    Retryability::NotRetryable,
                    "the configured Forgejo credential is invalid",
                )
                .with_retry_hint(RetryHint::Reauthenticate));
            }
            Some(format!("token {token}"))
        } else {
            None
        };
        let response = if let Some(body) = body {
            let mut request = self
                .agent
                .post(url.as_str())
                .header("Accept", "application/json")
                .header("User-Agent", concat!("prism/", env!("CARGO_PKG_VERSION")))
                .header("Content-Type", "application/json");
            if let Some(authorization) = &authorization {
                request = request.header("Authorization", authorization);
            }
            request.send(body.as_slice())
        } else {
            let mut request = self
                .agent
                .get(url.as_str())
                .header("Accept", "application/json")
                .header("User-Agent", concat!("prism/", env!("CARGO_PKG_VERSION")));
            if let Some(authorization) = &authorization {
                request = request.header("Authorization", authorization);
            }
            request.call()
        }
        .map_err(|error| transport_error(operation, &error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
                    .map_err(|_| {
                        invalid_transport_response(operation, "Forgejo returned an invalid header")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_with_config()
            // Leave one byte beyond the detection byte so ureq can observe EOF
            // without reporting its own limit error for an exactly sized body.
            .limit(self.response_limit as u64 + 2)
            .reader()
            .read_to_end(&mut bytes)
            .map_err(|error| io_error(operation, &error))?;
        if bytes.len() > self.response_limit {
            return Err(RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::InvalidResponse,
                Retryability::NotRetryable,
                "Forgejo response exceeded the configured size limit",
            ));
        }
        let response = HttpResponse {
            status,
            headers,
            body: bytes,
        };
        if !(200..300).contains(&status) {
            return Err(response.status_error(operation));
        }
        Ok(response)
    }

    fn check_cancelled(&self, operation: RemoteOperation) -> Result<(), RemoteError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::Cancelled,
                Retryability::NotRetryable,
                "Forgejo operation was cancelled",
            ));
        }
        Ok(())
    }
}

fn requested_page_size(url: &Url) -> Option<usize> {
    url.query_pairs()
        .find(|(name, _)| name == "limit")
        .and_then(|(_, value)| value.parse().ok())
}

fn next_numbered_page(url: &Url) -> Option<Url> {
    let current = url
        .query_pairs()
        .find(|(name, _)| name == "page")?
        .1
        .parse::<u64>()
        .ok()?;
    let mut next = url.clone();
    let pairs = url
        .query_pairs()
        .map(|(name, value)| {
            let value = if name == "page" {
                current.checked_add(1)?.to_string()
            } else {
                value.into_owned()
            };
            Some((name.into_owned(), value))
        })
        .collect::<Option<Vec<_>>>()?;
    next.set_query(None);
    next.query_pairs_mut().extend_pairs(pairs);
    Some(next)
}

impl HttpResponse {
    pub(super) fn json<T: DeserializeOwned>(
        self,
        operation: RemoteOperation,
    ) -> Result<T, RemoteError> {
        serde_json::from_slice(&self.body).map_err(|_| {
            RemoteError::new(
                ProviderKind::Forgejo,
                operation,
                RemoteErrorClass::InvalidResponse,
                Retryability::NotRetryable,
                "Forgejo returned invalid JSON",
            )
            .with_status(self.status)
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn next_link(
        &self,
        api_base: &Url,
        operation: RemoteOperation,
    ) -> Result<Option<Url>, RemoteError> {
        let Some(link) = self.header("Link") else {
            return Ok(None);
        };
        let Some(target) = parse_next_link(link) else {
            return Ok(None);
        };
        let next = api_base
            .join(target)
            .map_err(|_| pagination_error(operation))?;
        if !same_origin(api_base, &next)
            || !next.username().is_empty()
            || next.password().is_some()
            || next.fragment().is_some()
        {
            return Err(pagination_error(operation));
        }
        Ok(Some(next))
    }

    fn status_error(&self, operation: RemoteOperation) -> RemoteError {
        let (class, retryability, message) = match self.status {
            401 => (
                RemoteErrorClass::Authentication,
                Retryability::NotRetryable,
                "Forgejo authentication failed",
            ),
            403 => (
                RemoteErrorClass::Authorization,
                Retryability::NotRetryable,
                "Forgejo denied the operation",
            ),
            404 => (
                RemoteErrorClass::NotFound,
                Retryability::NotRetryable,
                "Forgejo resource was not found",
            ),
            408 | 504 => (
                RemoteErrorClass::Timeout,
                Retryability::Retryable,
                "Forgejo request timed out",
            ),
            409 | 412 => (
                RemoteErrorClass::Conflict,
                Retryability::NotRetryable,
                "Forgejo rejected a conflicting operation",
            ),
            422 => (
                RemoteErrorClass::Validation,
                Retryability::NotRetryable,
                "Forgejo rejected the request",
            ),
            429 => (
                RemoteErrorClass::RateLimited,
                Retryability::Retryable,
                "Forgejo rate limit was reached",
            ),
            500..=599 => (
                RemoteErrorClass::Provider,
                Retryability::Retryable,
                "Forgejo service failed",
            ),
            300..=399 => (
                RemoteErrorClass::InvalidResponse,
                Retryability::NotRetryable,
                "Forgejo returned an unexpected redirect",
            ),
            _ => (
                RemoteErrorClass::Provider,
                Retryability::Unknown,
                "Forgejo returned an unsuccessful status",
            ),
        };
        let mut error = RemoteError::new(
            ProviderKind::Forgejo,
            operation,
            class,
            retryability,
            message,
        )
        .with_status(self.status);
        if matches!(self.status, 401 | 403) {
            error = error.with_retry_hint(RetryHint::Reauthenticate);
        } else if matches!(self.status, 409 | 412) {
            error = error.with_retry_hint(RetryHint::RefreshObservation);
        } else if self.status == 429 || self.status == 503 {
            error = error.with_retry_hint(
                self.header("Retry-After")
                    .and_then(parse_retry_after)
                    .map(RetryHint::After)
                    .unwrap_or(RetryHint::Backoff),
            );
        }
        error
    }
}

fn parse_next_link(link: &str) -> Option<&str> {
    link.split(',').find_map(|entry| {
        let (target, parameters) = entry.trim().split_once('>')?;
        let target = target.strip_prefix('<')?;
        parameters
            .split(';')
            .map(str::trim)
            .any(|parameter| {
                parameter
                    .strip_prefix("rel=")
                    .map(|value| {
                        value
                            .trim_matches('"')
                            .split_ascii_whitespace()
                            .any(|rel| rel == "next")
                    })
                    .unwrap_or(false)
            })
            .then_some(target)
    })
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn configuration_error(message: &str) -> RemoteError {
    RemoteError::new(
        ProviderKind::Forgejo,
        RemoteOperation::DiscoverRepository,
        RemoteErrorClass::Configuration,
        Retryability::NotRetryable,
        message,
    )
}

fn pagination_error(operation: RemoteOperation) -> RemoteError {
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::NotRetryable,
        "Forgejo returned an invalid cross-origin pagination link",
    )
}

fn transport_error(operation: RemoteOperation, error: &ureq::Error) -> RemoteError {
    if let ureq::Error::Io(error) = error {
        return io_error(operation, error);
    }
    if matches!(error, ureq::Error::Timeout(_)) {
        return io_error(
            operation,
            &std::io::Error::new(ErrorKind::TimedOut, "request timeout"),
        );
    }
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        RemoteErrorClass::Transport,
        Retryability::Retryable,
        "Forgejo transport failed",
    )
    .with_retry_hint(RetryHint::Backoff)
}

fn invalid_transport_response(operation: RemoteOperation, message: &str) -> RemoteError {
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::NotRetryable,
        message,
    )
}

fn io_error(operation: RemoteOperation, error: &std::io::Error) -> RemoteError {
    let timeout = matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock);
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        if timeout {
            RemoteErrorClass::Timeout
        } else {
            RemoteErrorClass::Transport
        },
        Retryability::Retryable,
        if timeout {
            "Forgejo request timed out"
        } else {
            "Forgejo transport failed"
        },
    )
    .with_retry_hint(RetryHint::Backoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after("12"), Some(Duration::from_secs(12)));
    }

    #[test]
    fn recognizes_only_next_link_relation() {
        let link = "<https://example.test/api/v1/pulls?page=1>; rel=\"prev\", <https://example.test/api/v1/pulls?page=3>; rel=\"next\"";
        assert_eq!(
            parse_next_link(link),
            Some("https://example.test/api/v1/pulls?page=3")
        );
    }
}
