# Remote Hosting Adapters for Prism

Research performed 2026-07-29 against GitHub, GitLab, Forgejo, and the live
Codeberg deployment.

## Recommendation

Build one provider-neutral **change-request domain** around Prism's existing
workflows, then put GitHub, GitLab, and Forgejo behind provider adapters.
Codeberg should be a known Forgejo instance profile, not a fourth protocol
adapter. Preserve provider-native identifiers and states alongside normalized
facts, and make every optional operation capability-gated.

Do not define parity as "all providers have pull requests, reviews, and CI."
That statement is superficially true but unsafe for Prism's automation:

- GitHub pull requests, GitLab merge requests, and Forgejo pull requests have
  different identifiers, state machines, review models, CI associations, policy
  surfaces, and merge guards.
- GitLab discussions have a documented resolvable-thread API. GitHub review
  threads expose resolution through GraphQL. Forgejo review comments carry
  resolver metadata in its generated schema, but the public API examined here
  has no corresponding review-conversation resolution operation. [GitHub
  resolve review thread][github-resolve-thread] [GitLab
  discussions][gitlab-discussions] [Codeberg OpenAPI][codeberg-openapi]
- GitHub merge queues and GitLab merge trains are distinct policy mechanisms.
  Forgejo does not expose an equivalent queue primitive in the API examined.
- Codeberg runs Forgejo but controls its own version, limits, enabled features,
  runners, and policy. Its first-party CI documentation says hosted Actions are
  limited and recommends Woodpecker when Codeberg-hosted CI is required.

The safe initial support target is therefore:

1. Preserve existing GitHub behavior behind the new boundary without rewriting
   its transport.
2. Add GitLab for change-request discovery, details, discussions, pipelines,
   failed-job traces, policy observation, creation, and guarded merge.
3. Add Forgejo and Codeberg for discovery, reviews, statuses/Actions, branch
   protection, creation, and guarded merge, while marking conversation
   resolution and merge queue as unsupported.
4. Hide or disable an action when the selected remote does not advertise the
   required capability. Unknown or stale policy must continue to block
   automatic merge, as it does today.

## Prism's Actual Contract

Prism currently does considerably more than render a pull-request list. The
requirements establish this desired adapter contract, while `src/github.rs`
shows which parts are implemented today:

- Discover a change request associated with a local branch and cache summary
  and details independently without converting transient failure into absence.
- List open remote change requests and open one in a deterministic worktree.
- Read title, body, URL, lifecycle, draft status, source/target branches, exact
  source SHA, requested reviewers, aggregate review decision, mergeability,
  comments, formal reviews, inline conversations, changed files, checks, and CI
  failures.
- Distinguish top-level context from actionable review bodies and inline
  feedback; retain stable conversation IDs and resolution state.
- Produce bounded failed-CI log tails as untrusted prompt input.
- Observe required approvals, required checks, conversation resolution,
  up-to-date requirements, and queue requirements. Unknown or stale policy is
  not merge permission.
- Push and create a change request, resolve only conversations used by a managed
  repair, and merge only after immediately revalidating repository, branch,
  target, policy, and expected source SHA.
- Support forks and an `origin`/`upstream` choice.

Issues and labels are useful future capabilities, but they are not part of the
current stabilization loop. They should not distort the first adapter
interface.

The existing GitHub implementation is not yet a complete reference
implementation of that contract. In particular, its summary query requests the
first 100 pull requests without pagination, its branch-protection query requests
the first 20 rules without pagination, it does not query rulesets, and
`merge_queue_required` is currently always stored as `false`. Extracting an
adapter should preserve shipped behavior first, but contract tests should make
these limitations explicit rather than canonizing them as provider-neutral
semantics.

## Codebase Change Map

The current provider seam is `src/github.rs`, but GitHub names and data shapes
cross several layers. A migration needs coordinated changes in these areas:

| Area | Current coupling | Recommended change |
|---|---|---|
| Domain and transport | `src/github.rs` owns GitHub DTOs, subprocess calls, normalized PR models, cache state, and SQLite access in one module | Extract provider-neutral change-request, observation, policy, and error types; leave GraphQL/`gh` details in a GitHub adapter |
| Polling | `src/actions/polling.rs` checks `github_remote_configured`, refreshes GitHub policy, and calls GitHub summary/detail functions directly | Resolve a repository's configured adapter once, then schedule provider-neutral summary and detail operations |
| User actions | `src/actions/pull_requests.rs` directly creates PRs, resolves GitHub thread IDs, lists PRs, and selects `origin`/`upstream` using GitHub repository strings | Route create/open/fetch/resolve operations through the adapter and gate unsupported actions with capabilities |
| Stabilization | `src/auto_flow/stabilization_observe.rs` and `stabilization_execute.rs` carry `github_remote`, `pr_number: u64`, GitHub merge-state wording, and direct GitHub refresh/merge calls | Use opaque change-request identity and provider-neutral facts; keep mutation authorization provider-neutral and execution provider-owned |
| CI prompts | `src/ci.rs` and prompt defaults assume GitHub Actions and a PR number | Describe change requests and CI evidence generically while retaining provider and CI-system provenance |
| UI | `src/view/*`, `src/tui.rs`, and `RepoMainView::Github` expose GitHub module types and labels | Render provider-neutral models; retain provider-specific nouns such as "merge request" where they help the user |
| Persistence | `pr_cache` is keyed by branch, details use `(pr_number, head_sha)`, and policy uses an `owner/name` string | Add provider, canonical host, project identity, opaque native ID, display number, and head SHA to durable identities; migrate old rows as `github.com` |
| Configuration | `src/config.rs`, `schemas/config.schema.json`, and docs know `gh` but have no host/provider map, `glab`, or Forgejo credential source | Add known-host defaults and explicit self-host mappings; configure tools separately from secrets |
| Transport dependencies | `Cargo.toml` has no HTTP or URL parsing client | Preserve `gh`, use `glab` for an initial GitLab slice, and add a small TLS HTTP/URL layer only when implementing Forgejo |
| Diagnostics and redaction | Process descriptors and redaction know GitHub/`gh` patterns | Include provider, host, operation, version, capability, and classified failure while redacting every provider's token forms |

This should not be designed as a lowest-common-denominator interface. Expose the
full set of operations Prism needs, but make support for each optional operation
explicit. A provider returning `Unsupported` is materially different from an
empty result, an authorization failure, a stale observation, or a temporary
transport error.

## Capability Matrix

"Conditional" means the provider has an API but availability depends on
instance version, product tier, enabled repository units, CI installation, or
the particular policy configuration. "Gap" means no safe equivalent was found
in the primary sources examined.

| Prism capability | GitHub | GitLab | Forgejo | Codeberg |
|---|---|---|---|---|
| Repository identity and metadata | Native | Native | Native | Forgejo API |
| List/get associated change requests | Native PR REST/GraphQL | Native MR REST | Native PR REST | Forgejo API |
| Create change request | Native | Native | Native | Forgejo API |
| Draft state | Native | Native, with GitLab MR semantics | Native | Forgejo API |
| Formal review/approval state | Native reviews and `reviewDecision` | Native approvals, approval rules, and MR state | Native PR reviews | Forgejo API |
| Inline comments/conversations | Native review threads | Native MR discussions | Review comments are native | Forgejo API |
| Resolve an inline conversation | Native GraphQL mutation | Native discussion update | **Gap** in examined public API | **Gap** |
| Commit check/status rollup | Native Checks plus commit statuses | Pipelines, jobs, commit statuses, external status checks | Combined commit statuses plus Actions | Conditional on configured CI |
| Bounded failed-CI logs | Actions run/job logs | Job trace endpoint | Actions job log endpoint | Conditional; Actions hosting is limited and external CI may differ |
| Required review/check policy | Branch protection and rulesets | Conditional across protected branches, approval rules, and external checks | Branch protection exposes approvals and status contexts | Forgejo policy plus Codeberg configuration |
| Strict/up-to-date policy | Native branch protection/rulesets | Conditional merge checks/settings | Conditional branch protection/merge checks | Conditional |
| Merge queue | Native merge queue/rulesets | Merge trains, not the same model | **Gap** | **Gap** |
| Expected-head guarded merge | REST merge `sha` | MR merge `sha` | `head_commit_id` | Forgejo API |
| Fetch remote change into worktree | GitHub pull ref or source repo | MR ref or source project/branch | Source repository and branch from PR | Forgejo API |
| Issues and labels | Native | Native | Native | Forgejo API |
| First-party user CLI usable as API transport | `gh` | `glab` | No equivalent assumed by this design | No equivalent assumed by this design |

GitHub documents pull requests, reviews, review comments, checks, statuses,
workflow runs, branch protection, rulesets, and guarded merge in its REST and
GraphQL references. [GitHub pull requests][github-pulls] [GitHub
reviews][github-reviews] [GitHub review comments][github-review-comments]
[GitHub checks][github-checks] [GitHub statuses][github-statuses] [GitHub Actions
runs][github-actions-runs] [GitHub branch protection][github-branch-protection]
[GitHub rulesets][github-rulesets]

GitLab documents merge requests, discussions, approvals, protected branches,
pipelines, jobs/traces, commit statuses, external status checks, merge trains,
and the guarded merge `sha` parameter separately. [GitLab merge
requests][gitlab-mrs] [GitLab discussions][gitlab-discussions] [GitLab
approvals][gitlab-approvals] [GitLab protected
branches][gitlab-protected-branches]
[GitLab pipelines][gitlab-pipelines] [GitLab jobs][gitlab-jobs] [GitLab commit
statuses][gitlab-statuses] [GitLab external status checks][gitlab-external-checks]
[GitLab merge trains][gitlab-merge-trains]

Forgejo publishes an instance-generated Swagger UI and OpenAPI document. The
Codeberg document examined at `https://codeberg.org/swagger.v1.json` includes
repository PR list/get/create, PR reviews, combined commit status, branch
protection, Actions runs/jobs/logs, and PR merge. Its merge input includes
`head_commit_id`; its branch-protection representation includes
`required_approvals`, `status_check_contexts`, `block_on_rejected_reviews`,
`block_on_official_review_requests`, and `dismiss_stale_approvals`.
[Forgejo API usage][forgejo-api] [Codeberg Swagger UI][codeberg-swagger]
[Codeberg OpenAPI][codeberg-openapi]

Issues and labels are available from all three API families, but should remain
a later product capability. GitHub's repository issues endpoint can also return
pull requests because every pull request is an issue in the shared issue model;
clients must inspect the `pull_request` key instead of assuming every result is
an issue. GitLab uses project-local issue `iid` values and has project and group
label scopes. Forgejo exposes issue and label endpoints in the same generated
instance specification as its pull-request API. [GitHub issues][github-issues]
[GitHub labels][github-labels] [GitLab issues][gitlab-issues] [GitLab
labels][gitlab-labels] [Codeberg OpenAPI][codeberg-openapi]

## Provider Semantics

### GitHub

GitHub is the baseline rather than the universal model. Prism currently uses
`gh` commands and `gh api graphql`, including cursor pagination for review
threads. `gh api` supports REST and GraphQL, host selection, automatic REST
pagination, and cursor-driven GraphQL pagination. [GitHub CLI API][gh-api]

Important properties are:

- PR numbers are repository-local display and route identifiers; GraphQL node
  IDs identify review threads and mutations.
- A PR has distinct `OPEN`, `CLOSED`, and `MERGED` lifecycle states. Review
  decision, merge state, draft state, checks, and policy are separate facts.
- Check Runs and legacy commit Status Contexts coexist. Prism already combines
  them and must retain that behavior.
- Review conversations, review submissions, issue-style PR comments, and inline
  review comments are distinct resources. Only the conversation/thread is the
  unit Prism may resolve.
- The merge REST endpoint accepts an expected `sha`, allowing Prism to reject a
  source branch that changed after validation. [GitHub merge endpoint]
  [github-pulls]
- GitHub Enterprise Server uses a host-specific API base rather than
  `api.github.com`; `gh api --hostname` already supports authenticated alternate
  hosts. GitHub REST requests should send an explicit API version header.
  [GitHub REST versioning][github-versioning] [GitHub Enterprise REST]
  [github-enterprise-rest]

GitHub REST pagination is primarily `Link`-header based, while GraphQL uses
connections and cursors. Rate-limit state is reported in response headers and
through the rate-limit API; secondary limits and `Retry-After` must not be
collapsed into ordinary absence. [GitHub pagination][github-pagination]
[GitHub rate limits][github-rate-limits] [GitHub errors][github-errors]

### GitLab

GitLab's equivalent resource is a merge request, and its display identifier is
`iid`, not `id`. `id` is globally unique and `iid` is project-local; API routes
usually take the project plus MR `iid`. This distinction must survive in domain
identity rather than being squeezed into GitHub's `number`. [GitLab REST]
[gitlab-rest]

Important differences are:

- MR lifecycle, detailed merge status, draft/work-in-progress status, approval
  state, blocking discussions, pipeline status, and merge train state are
  separate observations. GitLab warns clients to use `detailed_merge_status`
  rather than the deprecated `merge_status`. [GitLab merge requests][gitlab-mrs]
- Threaded review feedback is represented by MR discussions containing notes.
  A discussion can be marked resolved through the discussion endpoint when it
  is resolvable. A note ID is not interchangeable with its discussion ID.
  [GitLab discussions][gitlab-discussions]
- Approval rules and protected-branch controls vary by tier and configuration.
  Approval state must be read as a provider result, not inferred solely by
  counting `approved_by`. [GitLab approvals][gitlab-approvals]
- CI is project pipeline/job oriented. The job trace endpoint returns the log
  needed for a bounded repair tail. A pipeline attached to an MR, a branch
  pipeline, a merged-results pipeline, and a merge-train pipeline are not
  necessarily interchangeable. [GitLab merge request pipelines]
  [gitlab-mr-pipelines] [GitLab jobs][gitlab-jobs]
- `PUT /projects/:id/merge_requests/:merge_request_iid/merge` accepts `sha` and
  fails when it does not match `HEAD`, which provides the expected-head guard
  Prism requires. The endpoint also has GitLab-specific automatic/queued merge
  behavior that should remain adapter-owned. [GitLab merge requests][gitlab-mrs]
- GitLab exposes synthetic MR refs such as
  `refs/merge-requests/:iid/head`, but a robust worktree implementation should
  also retain `source_project_id` and `source_branch` for forks. [GitLab MR refs]
  [gitlab-mr-refs]

GitLab REST is rooted at `/api/v4`. It supports personal, project, group, OAuth,
job, and other token forms with different permissions. Offset pagination is the
default and selected resources support keyset pagination; clients should follow
the returned `Link` rather than synthesize a next page. Self-managed instances
can set their own limits. [GitLab authentication][gitlab-auth] [GitLab REST]
[gitlab-rest] [GitLab rate limits][gitlab-rate-limits]

`glab` is first-party, supports GitLab.com, Dedicated, and Self-Managed, supports
multiple authenticated instances, and detects a hostname from Git remotes. It
offers MR, issue, CI, job, label, and generic API commands, making it the closest
incremental transport match for Prism's current `gh` integration. [GitLab CLI]
[glab]

### Forgejo

Forgejo's API is rooted at `/api/v1`; each instance serves Swagger at
`/api/swagger` and OpenAPI at `/swagger.v1.json`. It accepts basic auth and
Bearer or token authorization. It paginates with `page` and `limit`, emits
`Link` and `x-total-count`, and exposes instance paging limits at
`/api/v1/settings/api`. API compatibility is guaranteed within a Forgejo major
version, so discovery must retain the version from `/api/v1/version`.
[Forgejo API usage][forgejo-api] [Forgejo versions][forgejo-versions]

Forgejo provides broad functional coverage, but the adapter should not be
described as a GitHub adapter with a different base URL:

- Pull request, review, issue, label, status, Actions, and branch-protection
  objects have Forgejo/Gitea-derived schemas and native IDs.
- Combined commit status is the right initial check rollup. Actions runs, jobs,
  and per-job plaintext logs add richer CI evidence when Actions is enabled.
- Branch protection exposes approval count, required status contexts, rejected
  review blocking, official-review-request blocking, and stale-approval
  dismissal. These should map to explicit policy observations, not one
  `protected: bool`.
- `MergePullRequestOption.head_commit_id` supplies the source-SHA merge guard.
- Review-comment objects expose resolver information in the generated schema,
  but no matching public endpoint to resolve a review conversation was found.
  Prism must advertise `resolve_review_thread = false`; it must not fake
  resolution by deleting a comment or posting another comment.
- Forgejo Actions intentionally resembles but is not identical to GitHub
  Actions. Its own reference warns that undocumented GitHub behavior may not
  work. [Forgejo Actions reference][forgejo-actions]

Do not require or shell out to a non-first-party Forgejo CLI as part of the core
contract. A direct authenticated HTTP transport is the predictable baseline for
self-hosted Forgejo.

### Codeberg

Codeberg is a deployment of Forgejo, not a distinct API family. On the research
date its live endpoints reported:

```text
GET https://codeberg.org/api/v1/version
{"version":"16.0.0-dev-645-eeb81466+gitea-1.22.0"}

GET https://codeberg.org/api/v1/settings/api
{"max_response_items":50,"default_paging_num":30,
 "default_git_trees_per_page":1000,"default_max_blob_size":10485760}
```

These are observations, not constants. Prism should discover them and cache
them with a timestamp. The `+gitea-1.22.0` suffix declares Gitea API
compatibility according to Forgejo's numbering documentation. [Forgejo
versions][forgejo-versions]

Codeberg access tokens authenticate against the Forgejo API and must be handled
as secrets. Its first-party documentation currently says tokens grant full
account access. [Codeberg access tokens][codeberg-token]

CI capability must be discovered, not assumed. Codeberg documents that Actions
are disabled per repository by default, hosted Actions are currently limited,
and users can attach a self-hosted runner; it recommends Woodpecker when hosted
CI is needed. A successful Forgejo status/Actions implementation therefore does
not imply every Codeberg repository has logs Prism can fetch. [Codeberg Actions]
[codeberg-actions]

## False Common Denominators

The following normalizations would create bugs:

| Tempting abstraction | Why it is wrong | Required representation |
|---|---|---|
| `pr_number: u64` as identity | GitLab has project-local `iid` and global `id`; thread IDs may be strings or numbers | Repository identity plus provider-native opaque ID and optional display number |
| One `state: String` | Lifecycle, draft, mergeability, review, CI, and queue state evolve independently | Separate typed facts, each preserving unknown/native values |
| Comments are review threads | Top-level notes, review submissions, inline comments, and discussions have different authority and mutation IDs | Typed comment origin plus stable conversation ID and resolvability |
| Approval count equals approval policy | Rules, eligible approvers, stale approvals, Code Owners, and product tiers affect the result | Provider-computed review decision plus observed policy evidence |
| One pipeline equals PR checks | Commit statuses, check runs, branch pipelines, MR pipelines, merged-result pipelines, Actions, and external CI overlap | Check contexts keyed by source SHA and provenance |
| Protected branch means merge permitted | Required checks, conversations, strict updates, queues/trains, bypasses, and permissions remain | Independent policy observations with freshness and support state |
| Closed means merged | All providers distinguish unmerged closure from merge | `Open`, `Closed`, and `Merged` lifecycle plus native state |
| HTTP 404 means absent | It can mean hidden private data, wrong project/path encoding, unsupported endpoint, or genuine absence | Classified remote error; only affirmative list/get evidence yields authoritative absence |
| Codeberg is its own protocol | It tracks Forgejo while choosing a version and instance configuration | `Forgejo` provider plus a `codeberg.org` instance profile |

## Proposed Boundary

The adapter boundary should use Prism operations, not provider endpoint shapes.
One possible vocabulary is:

```rust
enum ProviderKind { GitHub, GitLab, Forgejo }

struct RemoteRepository {
    provider: ProviderKind,
    web_base: Url,
    api_base: Url,
    project_path: String,
    native_id: Option<String>,
}

struct ChangeRequestId {
    repository: RemoteRepositoryId,
    native_id: String,
    display_number: Option<u64>,
}

enum Lifecycle { Open, Closed, Merged, Unknown(String) }
enum Support { Supported, Unsupported, Conditional, Unknown }
enum Fact<T> { Known(T), Unsupported, Unknown, Stale(T), Failed(RemoteError) }

struct Capabilities {
    review_threads: Support,
    resolve_review_thread: Support,
    check_rollup: Support,
    ci_logs: Support,
    repository_policy: Support,
    guarded_merge: Support,
    merge_queue: Support,
}
```

The existing `PrObservationQuality` is the right idea and should become a
provider-neutral observation type. `Unsupported`, `not configured`, `not
authorized`, `not yet loaded`, `stale after failure`, and `authoritatively
absent` must remain distinct. A capability says whether an operation exists; an
observation says what Prism currently knows. Neither should be encoded as an
empty list or `false`.

Prefer narrow use-case methods over a mirror of every API:

```rust
trait RemoteHosting {
    fn capabilities(&self, repo: &RemoteRepository) -> Capabilities;
    fn list_change_requests(&self, repo: &RemoteRepository) -> Result<Page<_>, RemoteError>;
    fn change_request_details(&self, id: &ChangeRequestId) -> Result<ChangeDetails, RemoteError>;
    fn repository_policy(&self, repo: &RemoteRepository, target: &str) -> Result<Policy, RemoteError>;
    fn failed_ci(&self, id: &ChangeRequestId, head: &CommitId) -> Result<Vec<CiFailure>, RemoteError>;
    fn create_change_request(&self, request: CreateChangeRequest) -> Result<ChangeRequest, RemoteError>;
    fn resolve_review_thread(&self, id: &ChangeRequestId, thread: &ThreadId) -> Result<(), RemoteError>;
    fn merge(&self, request: GuardedMerge) -> Result<MergeResult, RemoteError>;
}
```

`GuardedMerge` should require the observed source SHA, target branch, and merge
method. The adapter may require more native guard data. The orchestration layer,
not the adapter, remains responsible for fresh policy, local cleanliness, local
checks, and revalidation. An adapter's successful response is then re-fetched
before Prism records the change request as merged.

Keep these concerns provider-owned:

- Endpoint paths, pagination, authentication headers, and CLI invocation.
- Native state decoding and forward-compatible unknown variants.
- Which CI run is relevant to the exact source SHA.
- How branch protection, rulesets, approval rules, or merge trains become policy
  evidence.
- How a fork source is fetched.
- Native error-body parsing and retry hints.

Keep these concerns provider-neutral:

- Cache freshness and generation ordering.
- Prompt trust boundaries and bounded logs.
- Poll scheduling and backoff decisions based on classified errors.
- Local Git checks, repair guards, and selected-thread bookkeeping.
- UI wording based on capabilities and known facts.

## Detection and Configuration

Remote URL parsing must be generic. The current parser only accepts four
`github.com` forms and exactly two path components. A replacement must handle:

- HTTPS, HTTP when explicitly allowed, `ssh://`, and SCP-like Git URLs.
- Ports, self-hosted domains, GitLab subgroups, and Forgejo organizations.
- Different fetch and push URLs.
- `origin`, `upstream`, and repositories where the change request targets a
  different project from its source.

Use known-host defaults only for `github.com`, `gitlab.com`, and `codeberg.org`.
An arbitrary hostname is not safely distinguishable as GitHub Enterprise,
GitLab, or Forgejo from its Git URL. Require a short explicit mapping, for
example:

```toml
[remote_hosts."git.example.com"]
provider = "forgejo"
web_url = "https://git.example.com"
# api_url is optional when the provider's standard relative path applies.
```

For known or configured Forgejo hosts, probe `/api/v1/version` and
`/api/v1/settings/api`. For GitLab, use `/api/v4/metadata` when authorized or
the version endpoint where available. For GitHub Enterprise, let authenticated
`gh` host configuration establish the host and API routing. Do not scan or send
credentials to unconfigured arbitrary hosts.

Credentials must not be stored in repository TOML or SQLite. Keep `gh` as the
GitHub credential broker and use `glab` initially for GitLab. Forgejo needs an
explicit secret source, such as an environment variable or OS credential-store
integration, for direct HTTP. Redact tokens, headers, dynamic URLs, response
bodies, and query values consistently with Prism's existing observability
requirements.

## Transport Strategy

Do not combine the domain extraction with a wholesale GitHub networking
rewrite. `Cargo.toml` currently has no HTTP client and Prism's GitHub behavior is
built around supervised `gh` subprocesses. The lowest-risk sequence is:

- Wrap the existing GitHub implementation as an adapter and preserve `gh`.
- Use `glab api` and focused `glab` commands for the first GitLab tracer bullet;
  it already handles multiple self-managed hosts and authentication.
- Add a synchronous, TLS-validating HTTP transport for Forgejo because no
  first-party end-user CLI is assumed. Keep it below the same provider-domain
  boundary rather than exposing HTTP to the UI or stabilization code.
- Reconsider direct HTTP for all providers only after behavior is covered by
  fixtures. Direct HTTP would improve access to status, pagination, rate-limit,
  retry, and request timing, but it is not necessary to prove the adapter seam.

Every transport must return a common classified error carrying provider,
operation, retryability, optional HTTP status/exit status, optional retry time,
and a safe diagnostic message. Authentication/authorization, rate limiting,
not-found, conflict/stale-head, validation, unsupported, transport failure, and
malformed response are distinct classes.

## Incremental Migration

1. Rename UI and orchestration concepts from GitHub/PR to remote/change request
   where they are truly generic. Keep provider wording in labels where users
   benefit from it.
2. Extract current summary, details, check, CI, policy, observation, and mutation
   inputs into a provider-neutral module. Preserve native values during the
   move; do not redesign and add providers in one patch.
3. Put the existing `src/github.rs` operations behind the boundary and run the
   current tests unchanged through the GitHub adapter.
4. Change cache keys from branch alone and `repo_remote = owner/name` to include
   provider, canonical host, project identity, change-request native ID, and
   head SHA. Migrate existing rows as `github.com` records without dropping
   cached data.
5. Replace GitHub-only remote parsing with generic remote discovery and explicit
   self-host mappings. Update `doctor` to report provider, host, API version,
   authentication, capabilities, and unavailable operations.
6. Implement a GitLab read-only tracer bullet: branch association, summary,
   details/discussions, exact-head pipeline/check state, and failed job tail.
   Then add policy, creation, guarded merge, and discussion resolution.
7. Implement Forgejo read-only support against two supported major versions and
   Codeberg. Add creation and guarded merge only after branch protection and
   exact-head checks are proven. Leave conversation resolution visibly disabled.
8. Add issue and label ports later, when Prism has a product workflow that needs
   them; do not pre-generalize the current stabilization interface.

## Testing Strategy

Use three layers; mocked command strings alone are insufficient.

### Contract fixtures

Store scrubbed provider responses for every adapter operation, including:

- Empty, single-page, and multi-page lists.
- Forks, renamed projects, subgroups, deleted source branches, and inaccessible
  source projects.
- Open, draft, closed-unmerged, merged, conflicted, and temporarily unknown
  merge states.
- Review approvals, changes requested, dismissed/stale reviews, unresolved and
  resolved discussions, and non-resolvable comments.
- Mixed legacy statuses/check runs, MR and branch pipelines, external statuses,
  Forgejo combined statuses, disabled Actions, and missing logs.
- Protected and unprotected targets, unknown/tier-hidden policy, queues/trains,
  and stale policy after a failed refresh.
- `401`, ambiguous `404`, rate limit, validation error, malformed JSON, partial
  pagination, stale-head conflict, timeout, and retry headers.
- Unknown enum values and extra fields to prove forward compatibility.

Run one provider-neutral behavior suite against all adapters: no failure becomes
absence; details stay associated with `(change request, head SHA)`; unsupported
facts do not become false; a merge without fresh policy and exact head is
rejected; only recorded actionable threads may be resolved.

### Local integration instances

Run pinned GitLab and Forgejo containers in CI or a scheduled compatibility job.
Seed repositories through APIs, create same-project and fork change requests,
post discussions/reviews, emit statuses, enable protection, and exercise
expected-head rejection. Test the oldest supported and current Forgejo major
because Forgejo guarantees compatibility within a major, not indefinitely
across majors. [Forgejo API usage][forgejo-api]

GitHub cannot be reproduced faithfully by a lightweight local clone. Keep
fixture tests for the GraphQL shape and run a tightly scoped opt-in smoke test
against a dedicated GitHub repository for review-thread resolution, rulesets,
Actions logs, and guarded merge.

### Live read-only probes

Use read-only scheduled probes for `github.com`, `gitlab.com`, and `codeberg.org`
to detect API/version drift without making release tests depend on public
services. Record only safe version, capability, schema, and latency metadata.
Codeberg's development build and instance limits make this especially useful.

## Risks and Open Questions

- **Forgejo conversation resolution:** treat as unsupported until an official,
  versioned endpoint is identified and tested. This blocks Prism's automatic
  post-repair resolution on Forgejo/Codeberg but not review display or repair.
- **Policy completeness:** GitHub rulesets and GitLab approval/merge policy can
  exceed simple branch protection, and permissions/tier may hide evidence. The
  conservative result is `Unknown`, never "no requirements."
- **External CI:** Codeberg commonly uses Woodpecker, while all providers can
  receive external commit statuses. Status rollup can establish pass/fail but
  fetching logs may require a future CI-service adapter separate from the
  hosting adapter.
- **Merge queue semantics:** GitHub queue and GitLab train should become native
  queue facts, not one Boolean that implies identical actions. Initial automatic
  merge may refuse both and direct the user to the provider UI.
- **Self-hosted compatibility:** provider and API base require configuration;
  server version and enabled capabilities require discovery. Hostname heuristics
  are insufficient.
- **Authentication UX:** `gh` and `glab` are established brokers. Forgejo needs a
  secure, non-TOML credential path before it can be enabled by default.

## Sources

[github-pulls]: https://docs.github.com/en/rest/pulls/pulls
[github-reviews]: https://docs.github.com/en/rest/pulls/reviews
[github-review-comments]: https://docs.github.com/en/rest/pulls/comments
[github-resolve-thread]: https://docs.github.com/en/graphql/reference/mutations#resolvereviewthread
[github-issues]: https://docs.github.com/en/rest/issues/issues
[github-labels]: https://docs.github.com/en/rest/issues/labels
[github-checks]: https://docs.github.com/en/rest/checks/runs
[github-statuses]: https://docs.github.com/en/rest/commits/statuses
[github-actions-runs]: https://docs.github.com/en/rest/actions/workflow-runs
[github-branch-protection]: https://docs.github.com/en/rest/branches/branch-protection
[github-rulesets]: https://docs.github.com/en/rest/repos/rules
[github-versioning]: https://docs.github.com/en/rest/about-the-rest-api/api-versions
[github-enterprise-rest]: https://docs.github.com/en/enterprise-server@latest/rest/using-the-rest-api/getting-started-with-the-rest-api
[github-pagination]: https://docs.github.com/en/rest/using-the-rest-api/using-pagination-in-the-rest-api
[github-rate-limits]: https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
[github-errors]: https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api
[gh-api]: https://cli.github.com/manual/gh_api
[gitlab-rest]: https://docs.gitlab.com/api/rest/
[gitlab-auth]: https://docs.gitlab.com/api/rest/authentication/
[gitlab-mrs]: https://docs.gitlab.com/api/merge_requests/
[gitlab-discussions]: https://docs.gitlab.com/api/discussions/
[gitlab-issues]: https://docs.gitlab.com/api/issues/
[gitlab-labels]: https://docs.gitlab.com/api/labels/
[gitlab-approvals]: https://docs.gitlab.com/api/merge_request_approvals/
[gitlab-protected-branches]: https://docs.gitlab.com/api/protected_branches/
[gitlab-pipelines]: https://docs.gitlab.com/api/pipelines/
[gitlab-mr-pipelines]: https://docs.gitlab.com/ci/pipelines/merge_request_pipelines/
[gitlab-jobs]: https://docs.gitlab.com/api/jobs/
[gitlab-statuses]: https://docs.gitlab.com/api/commits/#commit-status
[gitlab-external-checks]: https://docs.gitlab.com/api/status_checks/
[gitlab-merge-trains]: https://docs.gitlab.com/ci/pipelines/merge_trains/
[gitlab-mr-refs]: https://docs.gitlab.com/user/project/merge_requests/versions/#checkout-merge-requests-locally-through-the-head-ref
[gitlab-rate-limits]: https://docs.gitlab.com/security/rate_limits/
[glab]: https://docs.gitlab.com/cli/
[forgejo-api]: https://forgejo.org/docs/latest/user/api/usage/
[forgejo-versions]: https://forgejo.org/docs/latest/user/api/versions/
[forgejo-actions]: https://forgejo.org/docs/latest/user/actions/reference/
[codeberg-swagger]: https://codeberg.org/api/swagger
[codeberg-openapi]: https://codeberg.org/swagger.v1.json
[codeberg-token]: https://docs.codeberg.org/advanced/access-token/
[codeberg-actions]: https://docs.codeberg.org/ci/actions/
