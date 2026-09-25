# Workflows

A workflow is a short YAML file that describes the steps a task goes
through: agents, checks, approvals, policy gates, git operations and PR
handoff. Every run executes a **snapshot** of its workflow taken when the run
was created; the snapshot's SHA-256 is stored in the run and in the
`run_started` audit event, so later edits never change a run in flight.

Source of truth: `src/workflow/mod.rs`, `src/workflow/template.rs`,
`src/workflow/catalog.rs`, `src/checks/mod.rs`.

## File format

```yaml
version: 1                    # required, must be 1
name: implement-review        # required, ASCII letters, digits, '-', '_'
description: Implement, test, review, approve, draft PR.   # optional
defaults:                     # optional
  runner: codex               # default runner for agent steps
  timeout: 45m                # default step timeout
steps:                        # required, at least one
  - id: implement
    type: agent
    prompt: "{{task}}"
```

Unknown keys are rejected everywhere (typos fail loudly). Durations accept
`45m`, `2h`, `1h30m`, `90s`, `500ms`, `1d` or a bare number of seconds.

Step ids are 1–40 characters of `[A-Za-z0-9_-]` and must be unique.

## Common step fields

| Field | Meaning |
| --- | --- |
| `id` | Step identifier (required). |
| `type` | `agent`, `command`, `check`, `approval`, `policy`, `git`, `github_pr` (`parallel` is reserved and rejected). |
| `timeout` | Step timeout. Default: `defaults.timeout`, else `limits.agent_timeout` (agent steps, 45m) or `limits.command_timeout` (command/check steps, 15m). Always capped by the run's remaining `limits.max_runtime`. |
| `on_failure` | Retry routing, see below. |
| `continue_on_failure` | Record the failure (`step_failure_ignored` audit event) and continue. Mutually exclusive with `on_failure`. |
| `description` | Free text. |

## Step types

### `agent`

Runs an AI agent. With Herdr available the agent is interactive, in a real
Herdr pane (see [RUNNERS.md](RUNNERS.md)).

| Field | Default | Meaning |
| --- | --- | --- |
| `prompt` | required | Prompt template. |
| `runner` | see *Runner precedence* | `claude`, `codex`, `opencode`, `fake-success`, custom… |
| `skill` | none | Markdown skill prepended to the prompt. |
| `output` | `summary` | `summary` (free text / `{"summary": …}`), `review` (validated verdict JSON), `acceptance` (a judgement per acceptance criterion), `plan` (an ADR task plan; read-only), `conformance` (epic vs. ADR; read-only) or `contract` (tests written before the implementation; see below). |
| `gate` | `false` | With `output: review`: a verdict other than `approved` (or unparseable output) fails the step; with `output: acceptance`: any `not_met` criterion fails it — so `on_failure` can send the findings back to the implementer. Allowed only with `review` and `acceptance`. |
| `commit` | `git.auto_commit` (true) | Commit worktree changes after the step (hooks disabled, `.herdr-orchestrator/` never committed). |
| `policy_check` | `true` | Evaluate the worktree diff against policy after the step. |

What happens, in order: launch-policy check (`agent_start`) → agent runs →
output file read and validated → diff-based policy gate → commit → review
gate. A policy DENY on the launch or the diff blocks the run; a
REQUIRE_APPROVAL pauses it for a human.

#### The output-file contract

The orchestrator appends instructions to every agent prompt:

```text
Orchestrator instructions (herdr-orchestrator run #12 step `implement`, attempt 2):
- Working directory: /repo/.herdr-orchestrator/worktrees/12-add-caching (an isolated git worktree on branch herdr/12-add-caching). Stay inside it.
- Do not commit, push, open pull requests or rewrite git history; the orchestrator does that.
- When you are finished, write exactly one JSON object to this file (create directories as needed):
Output file: /repo/.herdr-orchestrator/worktrees/12-add-caching/.herdr-orchestrator/out/x-3f9a1c0b7d2e.json
Format:
{"summary": "<what you changed and why>", "status": "done" | "blocked", "notes": "<optional>"}
- After writing the file, stop and wait.
```

The output file lives in the worktree's `.herdr-orchestrator/out/`, which is
git-excluded and never committed. A stale file from an earlier attempt is
deleted before the agent starts; symlinks and files over 1 MiB are refused.

For `output: summary`, the `summary` field (or the raw file, or a tail of the
transcript if no file was written) becomes the step output. A missing file
sets `parse_failed` on the execution but does not fail the step.

For `output: review` the required format is:

```json
{
  "verdict": "approved" | "changes_requested" | "rejected",
  "summary": "<one paragraph>",
  "findings": [
    {"severity": "critical|high|medium|low|info",
     "file": "<path>", "line": 42,
     "description": "<what is wrong>", "recommendation": "<how to fix>"}
  ]
}
```

Validation: `verdict` must be one of the three values; `findings` must be an
array; each finding needs a known `severity` and a string `description`;
`line` must be a number or null. On success the parsed object is stored as
the execution's `structured` output and a `review_completed` event records
the verdict. If validation fails the raw output is kept, `parse_failed` is
set, and — only when `gate: true` — the step fails.

For `output: acceptance` (the task's criteria are in `{{acceptance}}`):

```json
{"criteria": [{"index": 1, "status": "met" | "not_met" | "unverifiable",
               "evidence": "<test name, file:line, or command and its result>"}],
 "verdict": "approved" | "changes_requested",
 "summary": "<one paragraph>"}
```

Every criterion `1..=N` of the task must be judged exactly once, and
`approved` with a `not_met` criterion is rejected. With `gate: true`, unmet
criteria (with the reviewer's evidence) become the implementer's
`{{feedback}}`. `unverifiable` is not a failure: it is shown in the
approval screen for a human. Audited as `acceptance_verified`.

`output: plan` and `output: conformance` are used by the epic workflows
(see *Epics* below). Both are **read-only**: if the worktree is dirty or
HEAD moved after the step, the step fails and the run is `blocked`
(`read_only_violation`). A `plan`, `acceptance` or `conformance` result
that cannot be validated always fails the step (with or without `gate`),
and the validation errors are the retry feedback.

#### `output: contract`

The agent writes executable tests for the task — not the implementation —
and reports them:

```json
{"files": ["tests/rate_limit.rs"],
 "check": ["cargo", "test", "--test", "rate_limit"],
 "criteria_map": {"1": ["over_limit_returns_429"], "2": ["limit_is_configurable"]},
 "summary": "…"}
```

Validation: at least one and at most 50 files, each a relative path that
exists inside the worktree (not under `.herdr-orchestrator/`); `check` an
argv array without templates (when missing, the named `tests` check is
used); when the task has acceptance criteria, every criterion needs at
least one test in `criteria_map`. An invalid result fails the step with the
errors as feedback.

Then, after the files are committed:

1. **Red proof.** The contract's check runs (with the normal policy
   pre-flight) and must **fail**. If it already passes, the contract proves
   nothing: the step fails with that explanation as feedback
   (`contract_not_red`).
2. **Lock.** The files' SHA-256 hashes, the check, the criteria map, the
   contract commit and the tail of the failing output are recorded in the
   run (`contract_locked`). From now on any change to a contract file is a
   DENY at the diff gate ("contract file changed after it was locked") and,
   for Claude, the `PreToolUse` hook refuses `Write`/`Edit` of those paths.
3. **Approval.** The first approval after the lock approves the contract:
   the approval screen shows the files, the check, the criteria map and the
   red output (`contract_approved`). If the approver edited contract files
   before saying yes, their version is re-hashed and locked and the contract
   is marked *amended* (`contract_amended`) — no new red proof: it is their
   call, and the receipt says so.
4. **Green and receipt.** A `check` with `contract: true` runs the locked
   check. The PR step refuses to push if a contract file changed, and the PR
   body gets a *Contract* section plus a machine-readable receipt block
   (`herdr-orchestrator-receipt: v1`, run, `contract_sha256`, contract
   commit, approval id, audit chain head). `herdr-orchestrator receipt verify
   <run|pr-url> [--at rev]` re-checks the files at a commit, the approval and
   the audit chain (exit 0/1).

### `command`

Runs an argv array directly (no shell).

```yaml
- id: build
  type: command
  command: ["cargo", "build", "--locked"]
  env: {RUST_LOG: info}          # optional; names [A-Za-z0-9_]
  cwd: crates/core               # optional; relative, inside the worktree
  timeout: 20m
```

`shell: true` runs a single script string through `/bin/sh -c`. It is
visible in the UI, audited, evaluated against policy per `&&`/`||`/`;`/`|`
segment, and the default policy requires approval for it.

```yaml
- id: smoke
  type: command
  shell: true
  command: ["make build && ./scripts/smoke.sh \"$HERDR_ORCH_WORKTREE\""]
```

If `command[0]` is literally `herdr-orchestrator`, it resolves to the
running binary. Before execution every command passes the policy pre-flight
(ALLOW → run, REQUIRE_APPROVAL → approval, DENY → run blocked). Output is
streamed (redacted) to `logs/<run>/<exec>.log`; the state keeps a bounded
excerpt.

### `check`

A verification command. Either an explicit `command` (same fields as above)
or a **named check**:

```yaml
- id: tests
  type: check
  check: tests          # tests | lint | security
```

`check:` and `command:` are mutually exclusive. `contract: true` (with
neither) runs the run's locked contract check — the step is `blocked` when
no contract was locked. Named checks resolve at run time in the worktree:

1. `checks.<name>` in configuration (argv array) wins. An empty list `[]`
   disables the check.
2. Otherwise auto-detection:

| Check | Detected command |
| --- | --- |
| `tests` | `Cargo.toml` → `cargo test --all`; `package.json` with a real `scripts.test` → `npm test --silent` (or `pnpm test` / `yarn test` / `bun test` by lockfile); `go.mod` → `go test ./...`; `pyproject.toml`/`pytest.ini`/`setup.cfg`/`tox.ini` → `python3 -m pytest -q`; `Makefile` `test:` target → `make test` |
| `lint` | Cargo → `cargo clippy --all-targets -- -D warnings`; `scripts.lint` → `<npm\|pnpm\|yarn\|bun> run lint`; Go → `go vet ./...`; Python with `ruff` installed → `ruff check .`; `Makefile` `lint:` → `make lint` |
| `security` | Cargo with `cargo-audit` installed → `cargo audit`; `package-lock.json` → `npm audit --audit-level=high`; Python with `pip-audit` → `pip-audit`; Go with `govulncheck` → `govulncheck ./...` |

If nothing is configured or detected the step is **skipped** (status
`skipped`, audit `step_skipped`) — not failed. `check_passed` /
`check_failed` events are emitted for check steps.

```yaml
# .ai/herdr-orchestrator/config.yaml
checks:
  tests: ["just", "test"]
  security: []            # disable
```

After a restart, an interrupted `check` is re-run automatically (checks are
declared read-only); an interrupted `command` is marked `needs_human`.

### `approval`

Pauses the run until a human decides.

```yaml
- id: approval
  type: approval
  reason: "Ship {{task_title}}?"
```

The request captures task, step, reason, agent/pane, repository, branch,
changed files and diff stat, prior step results, policy decisions and the
**pending action** (what the next step will do, e.g. "push branch
herdr/12-x to origin and open a draft PR"). Approve once, deny (the run
fails), or cancel the run. `limits.approval_timeout` (unset by default)
turns expiry into a denial — never an approval.

**Approval cover.** Approving an explicit approval step covers the
REQUIRE_APPROVAL decisions of the *immediately following* step (a gated
command, push or PR) as long as `HEAD` is unchanged since the approval. The
reuse is audited as `approval_reused`. A DENY is never covered. Likewise,
approving a `github_pr` step's own policy request covers the push it
performs.

### `policy`

`type: policy` (no other fields) evaluates every file changed so far plus
the aggregate diff, exactly like the automatic gate after agent steps.
Paths already approved in this run are not asked again unless their change
fingerprint (`change:+lines:-lines`) differs.

### `git`

```yaml
- id: commit
  type: git
  action: commit          # commit (default) | push
  message: "chore: {{task_title}}"   # optional, default git.commit_message
```

Commits never run repository hooks and never include `.herdr-orchestrator/`.
Pushes never force (`refs/heads/B:refs/heads/B`, no `+`) and are policy
gated (`git_push`, default: require approval).

### `github_pr`

```yaml
- id: pr
  type: github_pr
  draft: true             # default github.draft_pr (true)
  title: "{{task_title}}" # default: task title
  body: "..."             # default: generated summary (steps, policy, review verdict, audit command)
  base: main              # default: github.base, else the run's base ref (HEAD → main)
```

Requires `gh`. Idempotent: if a PR already exists for the branch it is
recorded and the step succeeds. Otherwise: policy (`github_pr`, default
require approval) → commit pending changes → push (if
`github.push_before_pr`) → `gh pr create`. Nothing merges automatically;
`github.auto_merge: true` is rejected by configuration validation.

### `parallel`

Reserved. Validation rejects it; use task variants (`--variants N`) for
parallel implementations.

## Retries: `on_failure`

```yaml
- id: tests
  type: check
  check: tests
  on_failure:
    retry_step: implement   # this step or an earlier one
    max_attempts: 2         # 1..10, default 1
    feedback: true          # default true
```

- A failure jumps back to `retry_step` and re-runs every step from there.
- Retries are bounded by `min(max_attempts, limits.max_retries)` (default
  `max_retries: 3`). Counters are keyed by the failing step and persisted in
  the run, so they survive restarts. When exhausted the run fails
  (`retries_exhausted`, then `run_failed`).
- Retrying into an `approval` or `github_pr` step is rejected at validation.
- With `feedback: true` the failure is delivered to the next agent step as
  `{{feedback}}`: `Feedback from the previous attempt: The `tests` step
  failed: <reason>` followed by the command, exit status and an output
  excerpt (first 20 lines, last 60 lines and up to 40 error-looking lines
  from the middle, capped at `output.feedback_max_bytes`, 12 000 by default),
  all redacted. For a gated review the feedback is the findings JSON. The
  feedback is consumed once an agent step succeeds.
- Policy DENY outcomes and ambiguous states never go through retry routing:
  a denied command would only be denied again.
- Timeouts fail the step; they are retried only if `on_failure` says so.
- A human `run retry [--from-step S]` restarts a stopped run and resets the
  retry counters of the steps from the restart point onward.

## Templates

Only plain `{{name}}` substitution exists: no expressions, filters, loops
or code. Unknown variables fail validation. Substituted values are never
re-expanded.

| Variable | Value | Trusted |
| --- | --- | --- |
| `{{task}}` | Task description | no |
| `{{task_title}}` | Task title | no |
| `{{task.id}}` | Task number | yes |
| `{{run.id}}` | Run id | yes |
| `{{repo.root}}` | Main repository root | yes |
| `{{worktree.path}}` | Run worktree | yes |
| `{{branch}}` | Run branch | yes |
| `{{base_sha}}` | Base commit | yes |
| `{{step.id}}` | Current step id | yes |
| `{{output_file}}` | Agent output file (agent steps) | yes |
| `{{previous.output}}` | Output of the step immediately before | no |
| `{{step.<id>.output}}` | Output of an earlier step `<id>` (≤ 8 KiB) | no |
| `{{feedback}}` | Retry feedback, empty on a first attempt | no |
| `{{acceptance}}` | The task's numbered acceptance criteria (epic tasks), empty otherwise | no |
| `{{contract}}` | The locked contract's files and check, empty before a contract is locked | no |

Trust rules (enforced by `workflow validate` and at load):

- Untrusted variables may appear in prompts, approval reasons, commit
  messages and PR text, but **never** in `command` argv or `env` (argument
  injection: text starting with `-` could become an option).
- `argv[0]` may not contain templates.
- `shell: true` scripts may not contain templates at all; use the
  environment variables below instead.
- `{{feedback}}`, `{{previous.output}}` and `{{step.<id>.output}}` render
  empty when they have no value yet; any other missing value is an error.

### Environment of command steps and headless agents

Children get an explicit environment: the `environment.inherit` allowlist
(plus a runner's `env_inherit` for agents), `environment.set`, the step's
`env`, and:

| Variable | Value |
| --- | --- |
| `HERDR_ORCH_RUN_ID` | run id |
| `HERDR_ORCH_TASK_ID` | task number |
| `HERDR_ORCH_STEP_ID` | step id |
| `HERDR_ORCH_REPO_ROOT` | repository root |
| `HERDR_ORCH_WORKTREE` | worktree path |
| `HERDR_ORCH_BRANCH` | branch |
| `HERDR_ORCH_BASE_SHA` | base commit |
| `HERDR_ORCH_PROMPT_FILE`, `HERDR_ORCH_OUTPUT_FILE` | headless/shell agents only |

## Runner precedence

For each agent step, the first of:

1. `--step-runner <step>=<runner>` on the task;
2. the variant runner (`--variant-runners a,b,c`), which applies to the
   **first agent step** of each variant only;
3. `--runner` on the task (all agent steps);
4. the step's `runner:`;
5. the workflow's `defaults.runner`;
6. configuration `defaults.runner` (built-in default `claude`).

(Internally 1 and 2 are both per-step overrides; an explicit
`--step-runner` for the first agent step wins over the variant runner.)

## Lookup order

Workflows (later overrides earlier, matched by the file's `name:`):

1. built-in (`workflows/*.yaml` compiled into the binary);
2. global: `$HERDR_PLUGIN_CONFIG_DIR/workflows/*.yaml`;
3. project: `<repo>/.ai/herdr-orchestrator/workflows/*.yaml`.

A value containing `/` or ending in `.yaml`/`.yml` is read as a file path.

Skills (`skill: name` → `name.md`, names `[A-Za-z0-9_-]`, ≤ 64 chars, files
≤ 256 KiB), highest priority first:

1. `<repo>/.ai/herdr-orchestrator/skills/`
2. `<repo>/.ai/skills/` (Cezar-compatible location)
3. `$HERDR_PLUGIN_CONFIG_DIR/skills/`
4. built-in: `implementation`, `code-review`, `security-review`, `acceptance-review`, `adr-planning`, `adr-conformance`

Skills and repository files are *instructions to agents*; treat skills from
untrusted repositories with the same suspicion as the code
(see THREAT_MODEL.md).

## Shipped workflows

### `quick-task`

```text
implement (agent, skill implementation) → done
```

### `implement-review`

```text
implement (codex) ──► tests (check, retry implement ×2) ──► review (claude, structured)
      ▲                        │ fail + feedback
      └────────────────────────┘
──► approval ──► pr (draft; covered by the approval)
```

### `secure-change`

```text
implement ──► tests (retry ×2) ──► lint (retry ×1) ──► security-scan (continue_on_failure)
   ▲                                                         │
   │                ┌──── changes_requested (findings) ◄─────┤
   └────────────────┴──── security-review (claude, gated, retry ×1)
──► policy (whole diff) ──► approval ──► pr (draft)
```

### `variant-review`

Run with variants, e.g.
`herdr-orchestrator task create "…" --workflow variant-review --variants 3 --variant-runners codex,codex,claude`.

```text
         ┌─► variant A: implement → tests → review ─┐
task ────┼─► variant B: implement → tests → review ─┼─► task compare (human)
         └─► variant C: implement → tests → review ─┘         │
                                                   task select <task> <run>
                                                   run pr <run>
```

Each variant has its own branch (`herdr/<n>-<slug>-va`, `-vb`, …),
worktree, logs and audit trail. Reviews give structured findings; nothing
picks a winner automatically.

### `dual-review`

```text
implement (codex) ──► tests (retry ×2) ──► review-claude (gated, retry ×2)
      ▲                                        │ findings
      └────────────────────────────────────────┤
                                       review-codex (gated, retry ×1)
──► approval ──► pr (draft)
```

Two independent reviewers from different providers, one after the other:
the second sees the code only after the first approves. (A `parallel` step
type is still not supported.)

### `epic-task`

The default workflow for tasks accepted from an epic plan.

```text
implement ──► tests (retry ×2) ──► [plan checks] ──► acceptance (claude, gated, retry ×2)
    ▲                                                   │ unmet criteria
    └───────────────────────────────────────────────────┘
──► approval (shows the criteria table and manual checks) ──► pr (draft)
```

Verification from the accepted plan is inserted as `check` steps before the
first reviewing step (`plan-<check>-N` for named checks not already in the
workflow, `plan-check-N` for commands), retrying the first agent step with
feedback. The run's workflow snapshot records the augmented YAML.

### `contract-first`

Tests first, locked once you approve them (skill `contract-writing`).

```text
contract (tests; check must FAIL) ──► approve-contract (you see files, check, criteria, red output; approving locks)
──► implement (contract locked) ──► contract-green (check contract: true, retry ×3)
──► tests (retry ×2) ──► review (claude) ──► approval ──► pr (draft, with receipt)
```

Works for epic tasks too (`epic.task_workflow: contract-first`: every
acceptance criterion must map to a test) and is the default for Sentry
incident tasks (the contract is the reproduction).

### `eval-task`, `update-verify`, `update-resolve` (internal)

- `eval-task` (used by `eval run`): the case's contract is committed and
  locked when the worktree is created; implement → `contract: true` check
  (retry ×2) → tests. No approvals, no PR.
- `update-verify` (used by `run update` after a clean merge): tests →
  approval → push to the existing PR.
- `update-resolve` (after a merge with conflicts): an agent resolves the
  conflicts → tests (retry ×2) → approval → push. The run diffs against the
  merged base, so what came from the base is not the branch's own change.

### `epic-plan`, `epic-conformance` (internal)

Used by `epic create` / `epic replan` and `epic verify`: one read-only agent
step each (`output: plan` with skill `adr-planning`, `output: conformance`
with skill `adr-conformance`), `commit: false`, retried with the validation
errors as feedback.

## Epics

```bash
herdr-orchestrator adr list                                   # ADRs, status, epic, drift
herdr-orchestrator epic create --from docs/adr/0007-rate-limiting.md [--runner claude]
herdr-orchestrator epic show E1                               # plan, dependencies, criteria, commands
herdr-orchestrator epic accept E1 [--only T1,T2] [--step-runner acceptance=claude]
herdr-orchestrator epic reject E1 [--only T3] | epic replan E1 --feedback "…" | epic edit E1
herdr-orchestrator epic verify E1                             # conformance review → proposed F1, F2…
```

- **Planning** (`epic create`, directory → one epic per active ADR without
  one): a read-only agent returns a plan — tasks with `key`, `title`,
  `description`, `acceptance` (≥ 1), `verification` (`checks`, `commands`
  as argv, `manual`), `depends_on`, `adr_refs`, `scope` globs, optional
  `workflow` and `risk`; plus `out_of_scope` and `open_questions`.
  Validation: unique keys, known dependencies, no cycles, at most
  `epic.max_tasks` (default 12), known workflow and check names, commands
  without templates. A plan is never partly accepted.
- **Accepting** creates tasks in dependency order with the plan's scope
  (`task-scope` approval for changes outside it), acceptance criteria,
  manual checks and extra checks; runner options apply to all of them.
  Accepting is how you approve the plan's commands; they are still
  policy-checked when they run. Dependencies of an accepted key must be
  accepted too.
- **Dependencies** (`epic.dependency_mode`): `merged` waits until the
  dependency's commits are in the base branch (local check; merge the PR
  and update the local branch). `stacked` branches from the single unmerged
  dependency and its PR targets that branch. A failed or cancelled
  dependency marks the dependent `blocked`; `task unblock <id>` starts it
  anyway. The dashboard shows why a queued task waits.
- **Conformance** (`epic verify`): a read-only agent starts from the base
  branch, gets the ADR and the list of task branches and PRs, marks each
  Decision/Consequences statement `covered`/`partial`/`missing`, and
  proposes follow-ups, which become open plan tasks `F1…`.
- **Drift**: the ADR hash is stored; `adr list`, `epic list/show` say when
  the ADR changed. `epic replan` plans again with the current text; accepted
  tasks keep their keys.

In the pane: `e` opens the Epics screen (`enter` plan, `y` accept open
tasks, `n` reject, `g` re-plan, `v` verify).

## Project memory in prompts

Agent steps with `output: summary`, `contract` or `plan` get a "Prior work in
this repository" section appended to their prompt on a fresh start (not on
a retry that reuses the same agent, which already has it). Review,
acceptance and conformance steps never get it. See `memory.*` in the
configuration, `herdr-orchestrator history --task N` to preview it, and
`note add` for notes. A `summary` result may carry
`"notes_for_others": ["…"]`: up to five short facts other agents working in
the repository should know; they are stored as notes scoped to the files
the run changed.

## Validation

```bash
herdr-orchestrator workflow list
herdr-orchestrator workflow show implement-review
herdr-orchestrator workflow validate                   # every workflow in scope
herdr-orchestrator workflow validate my-flow.yaml other-name
```

`validate` parses and validates each workflow (schema, ids, templates and
trust rules, retry targets), checks that every named runner resolves and
every skill exists, prints `OK`/`ERROR` per workflow and exits 1 if any
failed. `herdr-orchestrator plan "task text" --workflow W` (or
`--dry-run task create …`) shows what a workflow would do, with policy
decisions, without creating anything.
