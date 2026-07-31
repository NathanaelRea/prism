#!/usr/bin/env bash
set -euo pipefail

readonly GITLAB_IMAGE="docker.io/gitlab/gitlab-ce:18.2.0-ce.0@sha256:182613a8f82544404884024c7f9acbd48f3c191349a52df25f2044761b0e45a1"
readonly FORGEJO_IMAGE="codeberg.org/forgejo/forgejo:11.0.1@sha256:53d3a4ec77f79fcf8f71b959fdf9fc59235a1dc8e064f5acd24edb0cc8b70325"
readonly GLAB_VERSION="1.58.0"
readonly GLAB_AMD64_SHA256="fbd3bf37a0e587cb36b295cea6957bf3a8578692c0fa08ab7a1ee827687da557"
readonly GLAB_ARM64_SHA256="f6130f699d67b4c4ba76ec7d1d736d08d93a5864fd3aaab57c5d136fb03012f4"

usage() {
  printf 'usage: %s gitlab|forgejo|self-test\n' "$0" >&2
}

fail() {
  printf 'FAIL: %s compatibility: %s\n' "$provider" "$1" >&2
  exit 1
}

skip() {
  printf 'SKIP: %s compatibility: %s\n' "$provider" "$1" >&2
}

unavailable() {
  skip "$1"
}

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    unavailable "required tool '$1' is unavailable"
    exit 0
  }
}

test_unavailable_prerequisite_skip() {
  provider="mode-test"
  local output
  output="$(unavailable "fixture unavailable" 2>&1)"
  [[ "$output" == "SKIP: mode-test compatibility: fixture unavailable" ]] \
    || fail "unavailable prerequisite did not report SKIP"
  printf 'PASS: unavailable prerequisites skip compatibility runs\n'
}

wait_for_json() {
  local url="$1"
  local filter="$2"
  local attempts="$3"
  local delay="$4"
  local body
  body="$(mktemp "$tmp_dir/response.XXXXXX")"

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if curl --disable --silent --fail --max-time 10 --max-filesize 1048576 \
      --output "$body" "$url" \
      && jq --exit-status "$filter" "$body" >/dev/null; then
      return 0
    fi
    sleep "$delay"
  done
  return 1
}

run_container() {
  local image="$1"
  local container_port="$2"
  shift 2

  container_name="prism-remote-compat-${provider}-$$"
  docker run --detach --rm \
    --name "$container_name" \
    --publish "127.0.0.1::${container_port}" \
    "$@" \
    "$image" >/dev/null

  local mapping
  mapping="$(docker port "$container_name" "${container_port}/tcp")"
  host_port="${mapping##*:}"
  [[ "$host_port" =~ ^[0-9]+$ ]] || fail "could not determine the container's local port"
}

install_glab() {
  local architecture archive checksum
  case "$(uname -m)" in
    x86_64)
      architecture="amd64"
      checksum="$GLAB_AMD64_SHA256"
      ;;
    aarch64 | arm64)
      architecture="arm64"
      checksum="$GLAB_ARM64_SHA256"
      ;;
    *)
      fail "glab $GLAB_VERSION has no configured checksum for $(uname -m)"
      ;;
  esac
  archive="$tmp_dir/glab.tar.gz"
  curl --disable --silent --show-error --fail --location \
    --output "$archive" \
    "https://gitlab.com/gitlab-org/cli/-/releases/v${GLAB_VERSION}/downloads/glab_${GLAB_VERSION}_linux_${architecture}.tar.gz"
  printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check >/dev/null
  tar -xzf "$archive" -C "$tmp_dir"
  glab_path="$tmp_dir/bin/glab"
  [[ -x "$glab_path" ]] || fail "pinned glab archive did not contain bin/glab"
}

gitlab_api() {
  "$glab_path" api --hostname "$gitlab_glab_host" "$@"
}

gitlab_project_id() {
  gitlab_api "projects/$(jq -rn --arg value "$1" '$value | @uri')" | jq -er '.id'
}

wait_for_gitlab_project() {
  local project_path="$1"
  local encoded
  encoded="$(jq -rn --arg value "$project_path" '$value | @uri')"
  for ((attempt = 1; attempt <= 60; attempt++)); do
    if gitlab_api "projects/$encoded" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  fail "fork project '$project_path' did not become ready"
}

gitlab_create_branch_commit() {
  local project_id="$1"
  local branch="$2"
  local content="$3"
  gitlab_api "projects/$project_id/repository/branches" --method POST \
    --raw-field "branch=$branch" --raw-field 'ref=main' >/dev/null
  gitlab_api "projects/$project_id/repository/files/compat.txt" --method POST \
    --raw-field "branch=$branch" --raw-field "content=$content" \
    --raw-field "commit_message=Seed $branch" >/dev/null
}

seed_gitlab() {
  local target_group fork_group target_project same_mr fork_mr same_iid same_sha

  target_group="$(gitlab_api groups --method POST --raw-field 'name=Prism Target' \
    --raw-field 'path=prism-target' --raw-field 'visibility=private')"
  fork_group="$(gitlab_api groups --method POST --raw-field 'name=Prism Fork' \
    --raw-field 'path=prism-fork' --raw-field 'visibility=private')"
  target_project="$(gitlab_api projects --method POST --raw-field 'name=compat-target' \
    --raw-field 'path=compat-target' --raw-field "namespace_id=$(jq -r '.id' <<<"$target_group")" \
    --raw-field 'visibility=private' --raw-field 'initialize_with_readme=true' \
    --raw-field 'default_branch=main' --raw-field 'default_branch_protection=0')"
  gitlab_target_id="$(jq -er '.id' <<<"$target_project")"

  gitlab_api "projects/$gitlab_target_id/fork" --method POST \
    --raw-field "namespace_id=$(jq -r '.id' <<<"$fork_group")" >/dev/null
  wait_for_gitlab_project 'prism-fork/compat-target'
  gitlab_fork_id="$(gitlab_project_id 'prism-fork/compat-target')"

  gitlab_create_branch_commit "$gitlab_target_id" 'same-seeded' 'same-project fixture'
  gitlab_create_branch_commit "$gitlab_target_id" 'adapter-create' 'adapter create fixture'
  gitlab_create_branch_commit "$gitlab_fork_id" 'fork-seeded' 'fork fixture'

  same_mr="$(gitlab_api "projects/$gitlab_target_id/merge_requests" --method POST \
    --raw-field 'source_branch=same-seeded' --raw-field 'target_branch=main' \
    --raw-field 'title=compat same-project seeded' --raw-field 'description=adapter details fixture')"
  same_iid="$(jq -er '.iid' <<<"$same_mr")"
  same_sha="$(jq -er '.sha' <<<"$same_mr")"
  fork_mr="$(gitlab_api "projects/$gitlab_fork_id/merge_requests" --method POST \
    --raw-field 'source_branch=fork-seeded' --raw-field 'target_branch=main' \
    --raw-field "target_project_id=$gitlab_target_id" \
    --raw-field 'title=compat fork seeded' --raw-field 'description=fork identity fixture')"
  jq -e --argjson target "$gitlab_target_id" --argjson source "$gitlab_fork_id" \
    '.target_project_id == $target and .source_project_id == $source' <<<"$fork_mr" >/dev/null \
    || fail "GitLab did not create a cross-project fork merge request"

  gitlab_api "projects/$gitlab_target_id/merge_requests/$same_iid/notes" --method POST \
    --raw-field 'body=compat seeded note' >/dev/null
  gitlab_discussion="$(gitlab_api "projects/$gitlab_target_id/merge_requests/$same_iid/discussions" \
    --method POST --raw-field 'body=compat exact discussion')"
  jq -e '.id | type == "string" and length > 0' <<<"$gitlab_discussion" >/dev/null \
    || fail "GitLab did not return an exact discussion identity"
  gitlab_api "projects/$gitlab_target_id/statuses/$same_sha" --method POST \
    --raw-field 'state=success' --raw-field 'name=compat/status' \
    --raw-field 'description=pinned compatibility fixture' >/dev/null
  gitlab_api "projects/$gitlab_target_id/protected_branches/main" --method DELETE >/dev/null
  gitlab_api "projects/$gitlab_target_id/protected_branches" --method POST \
    --raw-field 'name=main' --raw-field 'push_access_level=0' \
    --raw-field 'merge_access_level=40' --raw-field 'allow_force_push=false' >/dev/null
  gitlab_api "projects/$gitlab_target_id" --method PUT \
    --raw-field 'only_allow_merge_if_all_discussions_are_resolved=true' >/dev/null
}

forgejo_api() {
  local token_name="$1"
  local method="$2"
  local endpoint="$3"
  local body="${4:-}"
  local token="${!token_name}"
  local -a arguments=(
    --disable --silent --show-error --fail-with-body
    --request "$method"
    --header @-
    "${forgejo_url}/api/v1/${endpoint}"
  )
  if [[ -n "$body" ]]; then
    arguments+=(--data "$body")
  fi
  if ! printf 'Authorization: token %s\nContent-Type: application/json\n' "$token" \
    | curl "${arguments[@]}"; then
    printf 'Forgejo fixture request failed: %s %s\n' "$method" "$endpoint" >&2
    return 1
  fi
}

wait_for_forgejo_repo() {
  local token_name="$1"
  local repository="$2"
  for ((attempt = 1; attempt <= 30; attempt++)); do
    if forgejo_api "$token_name" GET "repos/$repository" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  fail "Forgejo repository '$repository' did not become ready"
}

forgejo_create_branch_commit() {
  local token_name="$1"
  local repository="$2"
  local branch="$3"
  local content="$4"
  local body
  body="$(jq -nc --arg branch "$branch" \
    '{new_branch_name:$branch,old_branch_name:"main"}')"
  forgejo_api "$token_name" POST "repos/$repository/branches" "$body" >/dev/null
  body="$(jq -nc --arg branch "$branch" --arg content "$content" \
    '{branch:$branch,content:($content|@base64),message:("Seed "+$branch)}')"
  forgejo_api "$token_name" POST "repos/$repository/contents/compat.txt" "$body" >/dev/null
}

create_forgejo_user() {
  local username="$1"
  docker exec --user git "$container_name" sh -c \
    'password="$(od -An -N24 -tx1 /dev/urandom | tr -d " \n")"; forgejo admin user create --username "$1" --password "$password" --email "$1@example.invalid" --must-change-password=false' \
    sh "$username" >/dev/null
}

seed_forgejo() {
  local target same_pr same_index same_sha fork_pr body forgejo_review
  create_forgejo_user prism
  create_forgejo_user contributor
  forgejo_token="$(docker exec --user git "$container_name" forgejo admin user generate-access-token \
    --username prism --token-name prism-compat --scopes all --raw)"
  forgejo_contributor_token="$(docker exec --user git "$container_name" forgejo admin user generate-access-token \
    --username contributor --token-name prism-compat --scopes all --raw)"
  [[ -n "$forgejo_token" && -n "$forgejo_contributor_token" ]] \
    || fail "Forgejo did not create fixture access tokens"

  target="$(forgejo_api forgejo_token POST user/repos \
    '{"name":"compat-target","private":false,"auto_init":true,"default_branch":"main","readme":"Default"}')"
  jq -e '.full_name == "prism/compat-target"' <<<"$target" >/dev/null \
    || fail "Forgejo did not create the target repository"
  forgejo_api forgejo_contributor_token POST repos/prism/compat-target/forks '{}' >/dev/null
  wait_for_forgejo_repo forgejo_contributor_token contributor/compat-target

  forgejo_create_branch_commit forgejo_token prism/compat-target same-seeded 'same-project fixture'
  forgejo_create_branch_commit forgejo_contributor_token contributor/compat-target fork-seeded 'fork fixture'
  forgejo_create_branch_commit forgejo_contributor_token contributor/compat-target adapter-create 'adapter create fixture'

  same_pr="$(forgejo_api forgejo_token POST repos/prism/compat-target/pulls \
    '{"base":"main","head":"same-seeded","title":"compat same-project seeded","body":"adapter details fixture"}')"
  same_index="$(jq -er '.number' <<<"$same_pr")"
  same_sha="$(jq -er '.head.sha' <<<"$same_pr")"
  fork_pr="$(forgejo_api forgejo_token POST repos/prism/compat-target/pulls \
    '{"base":"main","head":"contributor:fork-seeded","title":"compat fork seeded","body":"fork identity fixture"}')"
  jq -e '.head.repo.full_name == "contributor/compat-target" and .base.repo.full_name == "prism/compat-target"' \
    <<<"$fork_pr" >/dev/null || fail "Forgejo did not preserve fork source/target identity"

  forgejo_api forgejo_token POST "repos/prism/compat-target/issues/$same_index/comments" \
    '{"body":"compat seeded comment"}' >/dev/null
  forgejo_review="$(forgejo_api forgejo_contributor_token POST \
    "repos/prism/compat-target/pulls/$same_index/reviews" \
    '{"body":"compat seeded review","event":"APPROVED"}')"
  jq -e '.body == "compat seeded review" and .state == "APPROVED"' <<<"$forgejo_review" >/dev/null \
    || fail "unsupported fixture: Forgejo did not create a submitted review"
  body="$(jq -nc --arg sha "$same_sha" \
    '{context:"compat/status",description:"pinned compatibility fixture",state:"success",target_url:"",sha:$sha}')"
  forgejo_api forgejo_token POST "repos/prism/compat-target/statuses/$same_sha" "$body" >/dev/null
  forgejo_api forgejo_token POST repos/prism/compat-target/branch_protections \
    '{"branch_name":"main","enable_push":false,"enable_force_push":false,"enable_merge_whitelist":false,"enable_status_check":false,"required_approvals":0,"block_on_rejected_reviews":false,"block_on_outdated_branch":false,"dismiss_stale_approvals":false}' >/dev/null
}

[[ $# -eq 1 ]] || {
  usage
  exit 2
}
provider="$1"
case "$provider" in
  gitlab)
    image="$GITLAB_IMAGE"
    ;;
  forgejo)
    image="$FORGEJO_IMAGE"
    ;;
  self-test)
    test_unavailable_prerequisite_skip
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
esac

for tool in cargo curl docker jq mktemp rm sleep; do
  require_tool "$tool"
done
if [[ "$provider" == "gitlab" ]]; then
  for tool in sha256sum tar uname; do
    require_tool "$tool"
  done
fi
docker info >/dev/null 2>&1 || {
  unavailable "Docker daemon is unavailable"
  exit 0
}
docker pull "$image" >/dev/null 2>&1 || {
  unavailable "pinned image '$image' is unavailable"
  exit 0
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/prism-remote-compat.XXXXXX")"
container_name=""
cleanup() {
  if [[ -n "$container_name" ]]; then
    docker rm --force "$container_name" >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

case "$provider" in
  gitlab)
    install_glab
    run_container "$image" 80 \
      --shm-size 256m \
      --env "GITLAB_OMNIBUS_CONFIG=external_url 'http://localhost'; puma['worker_processes'] = 0; sidekiq['concurrency'] = 5; prometheus_monitoring['enable'] = false;"
    gitlab_url="http://127.0.0.1:${host_port}"
    gitlab_host="127.0.0.1:${host_port}"
    gitlab_glab_host="127.0.0.1"
    if ! wait_for_json "$gitlab_url/api/v4/projects?simple=true&per_page=1" \
      'type == "array"' 90 10; then
      fail "GitLab API v4 did not become ready for pinned image '$image'"
    fi
    gitlab_token="$(docker exec "$container_name" gitlab-rails runner \
      'user = User.find_by_username!("root"); token = user.personal_access_tokens.create!(name: "prism-compat", scopes: [:api], expires_at: 1.day.from_now); token.set_token(SecureRandom.hex(32)); token.save!; print token.token')"
    [[ -n "$gitlab_token" ]] || fail "GitLab did not create a fixture access token"
    export GITLAB_TOKEN="$gitlab_token"
    export GLAB_CONFIG_DIR="$tmp_dir/glab-config"
    "$glab_path" config set check_update false --global >/dev/null
    "$glab_path" config set api_host "$gitlab_host" --host "$gitlab_glab_host" --global >/dev/null
    "$glab_path" config set api_protocol http --host "$gitlab_glab_host" --global >/dev/null
    seed_gitlab
    PRISM_REMOTE_COMPATIBILITY=1 \
      PRISM_GITLAB_COMPAT_URL="$gitlab_url" \
      PRISM_GITLAB_COMPAT_GLAB="$glab_path" \
      cargo test --lib remote::gitlab::tests::pinned_local_gitlab_adapter_compatibility \
        -- --ignored --exact --test-threads=1
    ;;
  forgejo)
    run_container "$image" 3000 \
      --env 'FORGEJO__database__DB_TYPE=sqlite3' \
      --env 'FORGEJO__security__INSTALL_LOCK=true' \
      --env 'FORGEJO__service__DISABLE_REGISTRATION=true' \
      --env 'FORGEJO__actions__ENABLED=false'
    forgejo_url="http://127.0.0.1:${host_port}"
    if ! wait_for_json "$forgejo_url/api/v1/version" \
      '.version | type == "string" and startswith("11.")' 60 2; then
      fail "Forgejo version API did not become ready for pinned image '$image'"
    fi
    if ! wait_for_json "$forgejo_url/api/v1/settings/api" 'type == "object"' 5 2; then
      fail "Forgejo API settings endpoint is incompatible with pinned image '$image'"
    fi
    seed_forgejo
    PRISM_REMOTE_COMPATIBILITY=1 \
      PRISM_FORGEJO_COMPAT_URL="$forgejo_url" \
      PRISM_FORGEJO_COMPAT_TOKEN="$forgejo_token" \
      cargo test --lib remote::forgejo::tests::pinned_local_forgejo_adapter_compatibility \
        -- --ignored --exact --test-threads=1
    ;;
esac

printf 'PASS: %s adapter exercised against pinned local container (%s)\n' "$provider" "$image"
