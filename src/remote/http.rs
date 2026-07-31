use std::collections::HashSet;
use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::flight_recorder::{self, ExternalCallCategory, ExternalCallOutcome, ExternalCallTrace};

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
    deadline: Duration,
    host_identity: String,
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
            host_identity: safe_host_identity(&api_base),
            api_base,
            agent,
            credential_environment: profile.credential_environment.clone(),
            response_limit,
            cancelled,
            deadline: timeout,
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
        let mut visited = HashSet::new();
        let mut expected_total = None;
        for _ in 0..MAX_PAGES {
            if !visited.insert(url.as_str().to_string()) {
                return Err(pagination_error(operation));
            }
            let response = self.send(operation, url.clone(), None)?;
            let next = response.next_link(&self.api_base, operation)?;
            let total = response.total_count(operation)?;
            if let Some(total) = total {
                if expected_total.is_some_and(|expected| expected != total) {
                    return Err(pagination_error(operation));
                }
                expected_total = Some(total);
            }
            let page = response.json::<Vec<T>>(operation)?;
            let full_page_without_link = next.is_none()
                && requested_page_size(&url).is_some_and(|limit| page.len() >= limit);
            let page_len = page.len();
            values.extend(page);
            if let Some(total) = expected_total {
                if values.len() > total {
                    return Err(pagination_error(operation));
                }
                if values.len() == total {
                    return Ok(values);
                }
                if page_len == 0 {
                    return Err(pagination_error(operation));
                }
            }
            if let Some(next) = next {
                url = next;
                continue;
            }
            if full_page_without_link || expected_total.is_some_and(|total| values.len() < total) {
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
        let request_bytes = body.as_ref().map_or(0, Vec::len);
        let mut trace = ExternalCallTrace::begin(
            ExternalCallCategory::Http,
            "forgejo.http.request",
            vec![
                flight_recorder::text("provider", "forgejo"),
                flight_recorder::text("operation", operation_label(operation)),
                flight_recorder::text("transport", "http"),
                flight_recorder::unsigned("deadline_ms", self.deadline.as_millis()),
                flight_recorder::unsigned("request_bytes", request_bytes),
                flight_recorder::text("host", &self.host_identity),
            ],
        );
        let mut status = None;
        let mut response_bytes = 0;
        let result = (|| {
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
            let response_status = response.status().as_u16();
            status = Some(response_status);
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    value
                        .to_str()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                        .map_err(|_| {
                            invalid_transport_response(
                                operation,
                                "Forgejo returned an invalid header",
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !(200..300).contains(&response_status) {
                return Err(HttpResponse {
                    status: response_status,
                    headers,
                    body: Vec::new(),
                }
                .status_error(operation));
            }
            if content_length(&headers).is_some_and(|length| length > self.response_limit) {
                return Err(response_too_large(operation));
            }
            let mut bytes = Vec::new();
            let mut reader = response
                .into_body()
                .into_with_config()
                // Leave one byte beyond the detection byte so ureq can observe EOF
                // without reporting its own limit error for an exactly sized body.
                .limit(self.response_limit as u64 + 2)
                .reader();
            let mut chunk = [0_u8; 8192];
            loop {
                self.check_cancelled(operation)?;
                let count = reader
                    .read(&mut chunk)
                    .map_err(|error| io_error(operation, &error))?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                response_bytes = bytes.len().min(self.response_limit);
                if bytes.len() > self.response_limit {
                    return Err(response_too_large(operation));
                }
            }
            Ok(HttpResponse {
                status: response_status,
                headers,
                body: bytes,
            })
        })();
        finish_http_trace(&mut trace, &result, status, response_bytes);
        result
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
            || !next.path().starts_with(api_base.path())
            || !next.username().is_empty()
            || next.password().is_some()
            || next.fragment().is_some()
        {
            return Err(pagination_error(operation));
        }
        Ok(Some(next))
    }

    fn total_count(&self, operation: RemoteOperation) -> Result<Option<usize>, RemoteError> {
        self.header("X-Total-Count")
            .map(|value| value.parse().map_err(|_| pagination_error(operation)))
            .transpose()
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
            405 if operation == RemoteOperation::MergeChangeRequest => (
                RemoteErrorClass::Unsupported,
                Retryability::NotRetryable,
                "Forgejo does not support the requested merge method",
            ),
            409 | 412 | 423 => (
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

fn safe_host_identity(url: &Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn operation_label(operation: RemoteOperation) -> &'static str {
    match operation {
        RemoteOperation::DiscoverRepository => "discover_repository",
        RemoteOperation::ListChangeRequests => "list_change_requests",
        RemoteOperation::ObserveChangeRequest => "observe_change_request",
        RemoteOperation::ObserveReviewThreads => "observe_review_threads",
        RemoteOperation::ResolveReviewThread => "resolve_review_thread",
        RemoteOperation::ObserveChecks => "observe_checks",
        RemoteOperation::LoadCiLogs => "load_ci_logs",
        RemoteOperation::ObserveChangedFiles => "observe_changed_files",
        RemoteOperation::ObserveRepositoryPolicy => "observe_repository_policy",
        RemoteOperation::FetchChangeRequest => "fetch_change_request",
        RemoteOperation::CreateChangeRequest => "create_change_request",
        RemoteOperation::MergeChangeRequest => "merge_change_request",
        RemoteOperation::ObserveMergeQueue => "observe_merge_queue",
    }
}

fn finish_http_trace(
    trace: &mut ExternalCallTrace,
    result: &Result<HttpResponse, RemoteError>,
    status: Option<u16>,
    response_bytes: usize,
) {
    let mut fields = vec![flight_recorder::unsigned("response_bytes", response_bytes)];
    if let Some(status) = status {
        fields.push(flight_recorder::unsigned("status", status));
    }
    let (outcome, retryability) = match result {
        Ok(_) => (ExternalCallOutcome::Success, "not_applicable"),
        Err(error) => (
            match error.class() {
                RemoteErrorClass::Timeout => ExternalCallOutcome::TimedOut,
                RemoteErrorClass::Cancelled => ExternalCallOutcome::Canceled,
                _ => ExternalCallOutcome::Failed,
            },
            match error.retryability() {
                Retryability::Retryable => "retryable",
                Retryability::NotRetryable => "not_retryable",
                Retryability::Unknown => "unknown",
            },
        ),
    };
    fields.push(flight_recorder::text("retryability", retryability));
    trace.finish(outcome, fields);
}

fn content_length(headers: &[(String, String)]) -> Option<usize> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, value)| value.parse().ok())
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
        "Forgejo returned invalid or incomplete pagination metadata",
    )
}

fn response_too_large(operation: RemoteOperation) -> RemoteError {
    RemoteError::new(
        ProviderKind::Forgejo,
        operation,
        RemoteErrorClass::InvalidResponse,
        Retryability::NotRetryable,
        "Forgejo response exceeded the configured size limit",
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
