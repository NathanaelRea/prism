#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s all|github|gitlab|codeberg|self-test\n' "$0" >&2
}

[[ $# -eq 1 ]] || {
  usage
  exit 2
}
selection="$1"
case "$selection" in
  all|github|gitlab|codeberg|self-test) ;;
  *)
    usage
    exit 2
    ;;
esac

for tool in jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'SKIP: public-host drift probes require %s\n' "$tool" >&2
    exit 0
  fi
done

if [[ "$selection" != "self-test" ]] && ! command -v curl >/dev/null 2>&1; then
  printf 'SKIP: public-host drift probes require curl\n' >&2
  exit 0
fi

fail_on_unavailable="${REMOTE_DRIFT_FAIL_UNAVAILABLE:-0}"
if [[ "$fail_on_unavailable" != "0" && "$fail_on_unavailable" != "1" ]]; then
  printf 'REMOTE_DRIFT_FAIL_UNAVAILABLE must be 0 or 1\n' >&2
  exit 2
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/prism-remote-drift.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT
observed_at="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
drift_detected=0
request_failure_detected=0
unavailable_detected=0

request() {
  local name="$1"
  local url="$2"
  local accept="$3"
  shift 3
  local body="$tmp_dir/${name}.json"
  local metrics

  if ! metrics="$(curl \
    --disable \
    --silent \
    --max-time 20 \
    --max-filesize 1048576 \
    --proto '=https' \
    --tlsv1.2 \
    --request GET \
    --header "Accept: $accept" \
    --header 'User-Agent: prism-read-only-drift-probe' \
    "$@" \
    --output "$body" \
    --write-out '%{http_code} %{time_total} %{size_download}' \
    "$url")"; then
    REQUEST_STATUS="000"
    REQUEST_LATENCY_MS=0
    REQUEST_BYTES=0
    REQUEST_BODY="$body"
    return 1
  fi

  read -r REQUEST_STATUS REQUEST_SECONDS REQUEST_BYTES <<<"$metrics"
  REQUEST_LATENCY_MS="$(jq -n --arg seconds "$REQUEST_SECONDS" '$seconds | tonumber * 1000 | round')"
  REQUEST_BODY="$body"
  [[ "$REQUEST_STATUS" == "200" ]]
}

classify_request_failure() {
  if [[ "$REQUEST_STATUS" == "000" || "$REQUEST_STATUS" == 5?? ]]; then
    REQUEST_FAILURE_OUTCOME=unavailable
    unavailable_detected=1
  else
    REQUEST_FAILURE_OUTCOME=http_error
    request_failure_detected=1
  fi
}

validate_codeberg_version() {
  jq --exit-status \
    'type == "object" and (.version | type == "string" and test("^[0-9A-Za-z.+_-]{1,80}$"))' \
    "$1" >/dev/null
}

validate_codeberg_settings() {
  jq --exit-status \
    'type == "object"
      and (.max_response_items | type == "number")
      and (.default_paging_num | type == "number")' \
    "$1" >/dev/null
}

validate_codeberg_repository() {
  jq --exit-status \
    'type == "object"
      and (.id | type == "number")
      and .full_name == "forgejo/forgejo"
      and .private == false
      and .has_pull_requests == true
      and (.default_branch | type == "string" and length > 0)' \
    "$1" >/dev/null
}

validate_codeberg_pulls() {
  jq --exit-status \
    'type == "array"
      and length <= 1
      and all(.[];
        (.number | type == "number" and . > 0 and floor == .)
        and (.head.sha | type == "string" and test("^[0-9a-f]{40}$")))' \
    "$1" >/dev/null
}

validate_codeberg_reviews() {
  jq --exit-status \
    'type == "array"
      and length <= 1
      and all(.[];
        type == "object"
        and (.id | type == "number")
        and (.state | type == "string"))' \
    "$1" >/dev/null
}

validate_codeberg_status() {
  local body="$1"
  local expected_sha="$2"
  jq --exit-status --arg expected_sha "$expected_sha" \
    'type == "object"
      and .sha == $expected_sha
      and (.state | type == "string")
      and (.total_count | type == "number")
      and (.statuses | type == "array" and length <= 1)
      and all(.statuses[];
        type == "object" and (.status | type == "string"))' \
    "$body" >/dev/null
}

schema_self_test() {
  local sha=0123456789abcdef0123456789abcdef01234567

  printf '%s\n' '{"version":"11.0.1"}' >"$tmp_dir/version.json"
  printf '%s\n' '{"max_response_items":50,"default_paging_num":30}' >"$tmp_dir/settings.json"
  printf '%s\n' '{"id":1,"full_name":"forgejo/forgejo","private":false,"has_pull_requests":true,"default_branch":"forgejo"}' >"$tmp_dir/repository.json"
  printf '%s\n' "[{\"number\":1,\"head\":{\"sha\":\"$sha\"}}]" >"$tmp_dir/pulls.json"
  printf '%s\n' '[]' >"$tmp_dir/empty-pulls.json"
  printf '%s\n' '[]' >"$tmp_dir/reviews.json"
  printf '%s\n' "{\"sha\":\"$sha\",\"state\":\"pending\",\"total_count\":1,\"statuses\":[{\"status\":\"pending\"}]}" >"$tmp_dir/status.json"
  printf '%s\n' "{\"sha\":\"$sha\",\"state\":\"pending\",\"total_count\":0,\"statuses\":[]}" >"$tmp_dir/empty-status.json"

  validate_codeberg_version "$tmp_dir/version.json"
  validate_codeberg_settings "$tmp_dir/settings.json"
  validate_codeberg_repository "$tmp_dir/repository.json"
  validate_codeberg_pulls "$tmp_dir/pulls.json"
  validate_codeberg_pulls "$tmp_dir/empty-pulls.json"
  validate_codeberg_reviews "$tmp_dir/reviews.json"
  validate_codeberg_status "$tmp_dir/status.json" "$sha"
  validate_codeberg_status "$tmp_dir/empty-status.json" "$sha"

  if validate_codeberg_status "$tmp_dir/status.json" aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; then
    printf 'self-test accepted a mismatched exact-head status\n' >&2
    exit 1
  fi
  printf 'PASS: remote drift probe schema self-test\n'
}

emit() {
  local provider="$1"
  local host="$2"
  local outcome="$3"
  local version="$4"
  local schema_ok="$5"
  local latency_ms="$6"
  local response_bytes="$7"
  shift 7
  jq --compact-output --null-input \
    --arg provider "$provider" \
    --arg host "$host" \
    --arg outcome "$outcome" \
    --arg observed_at "$observed_at" \
    --arg version "$version" \
    --argjson schema_ok "$schema_ok" \
    --argjson latency_ms "$latency_ms" \
    --argjson response_bytes "$response_bytes" \
    --args '$ARGS.named + {capabilities: $ARGS.positional}' \
    "$@"
}

probe_github() {
  local schema_ok=false
  local outcome=unavailable
  if request github-root "https://api.github.com/" "application/vnd.github+json" \
    --header 'X-GitHub-Api-Version: 2022-11-28'; then
    if jq --exit-status \
      'type == "object" and has("repository_url") and has("current_user_url")' \
      "$REQUEST_BODY" >/dev/null; then
      schema_ok=true
      outcome=ok
    else
      outcome=drift
      drift_detected=1
    fi
  else
    classify_request_failure
    outcome="$REQUEST_FAILURE_OUTCOME"
  fi
  emit github github.com "$outcome" 2022-11-28 "$schema_ok" \
    "$REQUEST_LATENCY_MS" "$REQUEST_BYTES" api_root repository_templates
}

probe_gitlab() {
  local schema_ok=false
  local outcome=unavailable
  if request gitlab-project \
    "https://gitlab.com/api/v4/projects/gitlab-org%2Fgitlab?simple=true" \
    "application/json"; then
    if jq --exit-status \
      'type == "object" and has("id") and has("path_with_namespace") and has("web_url")' \
      "$REQUEST_BODY" >/dev/null; then
      schema_ok=true
      outcome=ok
    else
      outcome=drift
      drift_detected=1
    fi
  else
    classify_request_failure
    outcome="$REQUEST_FAILURE_OUTCOME"
  fi
  emit gitlab gitlab.com "$outcome" api-v4 "$schema_ok" \
    "$REQUEST_LATENCY_MS" "$REQUEST_BYTES" public_project_read
}

codeberg_request() {
  local result=0
  if ! request "$@"; then
    result=1
  fi
  CODEBERG_REQUEST_COUNT=$((CODEBERG_REQUEST_COUNT + 1))
  CODEBERG_TOTAL_LATENCY=$((CODEBERG_TOTAL_LATENCY + REQUEST_LATENCY_MS))
  CODEBERG_TOTAL_BYTES=$((CODEBERG_TOTAL_BYTES + REQUEST_BYTES))
  if ((result != 0)); then
    if [[ "$REQUEST_STATUS" == "000" || "$REQUEST_STATUS" == 5?? ]]; then
      CODEBERG_UNAVAILABLE=1
    else
      CODEBERG_REQUEST_ERROR=1
    fi
  fi
  return "$result"
}

emit_codeberg() {
  local outcome="$1"
  local version="$2"
  local schema_ok="$3"
  local pull_count="$4"
  local review_count="$5"
  local status_count="$6"
  shift 6
  jq --compact-output --null-input \
    --arg provider forgejo \
    --arg host codeberg.org \
    --arg outcome "$outcome" \
    --arg observed_at "$observed_at" \
    --arg version "$version" \
    --argjson schema_ok "$schema_ok" \
    --argjson latency_ms "$CODEBERG_TOTAL_LATENCY" \
    --argjson response_bytes "$CODEBERG_TOTAL_BYTES" \
    --argjson requests "$CODEBERG_REQUEST_COUNT" \
    --argjson pull_requests "$pull_count" \
    --argjson reviews "$review_count" \
    --argjson statuses "$status_count" \
    --args \
    '$ARGS.named + {
      capabilities: $ARGS.positional,
      counts: {
        requests: $requests,
        pull_requests: $pull_requests,
        reviews: $reviews,
        statuses: $statuses
      }
    } | del(.requests, .pull_requests, .reviews, .statuses)' \
    "$@"
}

probe_codeberg() {
  local schema_ok=false
  local outcome=ok
  local version=unavailable
  local pull_count=0
  local review_count=0
  local status_count=0
  local pull_number=
  local head_sha=
  local -a capabilities=()

  CODEBERG_TOTAL_LATENCY=0
  CODEBERG_TOTAL_BYTES=0
  CODEBERG_REQUEST_COUNT=0
  CODEBERG_DRIFT=0
  CODEBERG_REQUEST_ERROR=0
  CODEBERG_UNAVAILABLE=0

  if codeberg_request codeberg-version \
    "https://codeberg.org/api/v1/version" "application/json"; then
    if validate_codeberg_version "$REQUEST_BODY"; then
      version="$(jq --raw-output '.version' "$REQUEST_BODY")"
      capabilities+=(version)
    else
      CODEBERG_DRIFT=1
    fi
  fi

  if codeberg_request codeberg-settings \
    "https://codeberg.org/api/v1/settings/api" "application/json"; then
    if validate_codeberg_settings "$REQUEST_BODY"; then
      capabilities+=(api_settings)
    else
      CODEBERG_DRIFT=1
    fi
  fi

  if codeberg_request codeberg-repository \
    "https://codeberg.org/api/v1/repos/forgejo/forgejo" "application/json"; then
    if validate_codeberg_repository "$REQUEST_BODY"; then
      capabilities+=(repository_identity)
    else
      CODEBERG_DRIFT=1
    fi
  fi

  if codeberg_request codeberg-pulls \
    "https://codeberg.org/api/v1/repos/forgejo/forgejo/pulls?state=all&sort=recentupdate&page=1&limit=1" \
    "application/json"; then
    if validate_codeberg_pulls "$REQUEST_BODY"; then
      pull_count="$(jq 'length' "$REQUEST_BODY")"
      capabilities+=(paginated_pull_requests)
      if ((pull_count > 0)); then
        pull_number="$(jq --raw-output '.[0].number' "$REQUEST_BODY")"
        head_sha="$(jq --raw-output '.[0].head.sha' "$REQUEST_BODY")"
      fi
    else
      CODEBERG_DRIFT=1
    fi
  fi

  if [[ -n "$pull_number" && -n "$head_sha" ]]; then
    if codeberg_request codeberg-reviews \
      "https://codeberg.org/api/v1/repos/forgejo/forgejo/pulls/${pull_number}/reviews?page=1&limit=1" \
      "application/json"; then
      if validate_codeberg_reviews "$REQUEST_BODY"; then
        review_count="$(jq 'length' "$REQUEST_BODY")"
        capabilities+=(pull_reviews)
      else
        CODEBERG_DRIFT=1
      fi
    fi

    if codeberg_request codeberg-status \
      "https://codeberg.org/api/v1/repos/forgejo/forgejo/commits/${head_sha}/status?page=1&limit=1" \
      "application/json"; then
      if validate_codeberg_status "$REQUEST_BODY" "$head_sha"; then
        status_count="$(jq '.statuses | length' "$REQUEST_BODY")"
        capabilities+=(exact_head_combined_statuses)
      else
        CODEBERG_DRIFT=1
      fi
    fi
  fi

  if ((CODEBERG_DRIFT != 0)); then
    outcome=drift
    drift_detected=1
  elif ((CODEBERG_REQUEST_ERROR != 0)); then
    outcome=http_error
    request_failure_detected=1
  elif ((CODEBERG_UNAVAILABLE != 0)); then
    outcome=unavailable
    unavailable_detected=1
  else
    schema_ok=true
  fi

  emit_codeberg "$outcome" "$version" "$schema_ok" \
    "$pull_count" "$review_count" "$status_count" "${capabilities[@]}"
}

case "$selection" in
  all)
    probe_github
    probe_gitlab
    probe_codeberg
    ;;
  github) probe_github ;;
  gitlab) probe_gitlab ;;
  codeberg) probe_codeberg ;;
  self-test) schema_self_test ;;
esac

if ((drift_detected != 0 || request_failure_detected != 0)); then
  exit 1
fi
if [[ "$fail_on_unavailable" == "1" ]] && ((unavailable_detected != 0)); then
  exit 1
fi
