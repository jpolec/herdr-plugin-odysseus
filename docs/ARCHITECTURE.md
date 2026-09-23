# herdr-orchestrator — Architecture

Status: MVP design, written before implementation and kept in sync with it.
Target host: **Herdr 0.9.0** (verified against the installed binary's
`herdr api schema --json`, protocol 22, and the Herdr source at 0.9.1).

## 1. What this is

`herdr-orchestrator` is a Herdr plugin that adds a governed orchestration layer
on top of Herdr:

```text
task → workflow → worktree → agents → checks → retries → review
     → policy → human approval → draft PR → auditable completion record
```

Herdr stays responsible for terminals, panes, agent processes, agent state
detection, workspaces and session persistence. The plugin never emulates a
terminal, never multiplexes PTYs, and never scrapes screens when Herdr exposes
structured state.

Functional reference: [open-mercato/cezar](https://github.com/open-mercato/cezar)
(MIT). Cezar was read for *concepts* (tasks, queue, worktree per task, YAML
workflows with `onFail` retry, variants, Markdown skills, GitHub handoff). No
Cezar source code is used. The main behavioral difference is deliberate: Cezar
drives agents headlessly (`claude --print --output-format stream-json`); this
plugin drives **interactive agents inside real Herdr panes**, so a human can
watch and take over any agent at any time.

## 2. Verified Herdr facts that shape the design

| Fact (verified) | Consequence |
| --- | --- |
| Plugins are out-of-process argv commands declared in `herdr-plugin.toml`: `[[build]]`, `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]`, `[[link_handlers]]`. No shell expansion. `min_herdr_version` is required. | Manifest is small and auditable; one Rust binary with subcommands serves every entrypoint. |
| Startup hooks are **one-shot**; there is no supervised plugin daemon in plugin v1. | We run our own detached, single-instance engine daemon (§4). Upstream request filed in `UPSTREAM_REQUESTS.md`. |
| Runtime env: `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONTEXT_JSON`, `HERDR_PLUGIN_EVENT[_JSON]`, `HERDR_PLUGIN_ACTION_ID`, `HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_WORKSPACE_ID/TAB_ID/PANE_ID`. | Paths come from env with documented fallbacks for headless CLI use. |
| No Herdr storage API. | We own persistence under `HERDR_PLUGIN_STATE_DIR`. |
| Socket API: newline-delimited JSON `{"id","method","params"}` → `{"id","result"}` / `{"id","error":{code,message}}`. | Thin typed client in `src/herdr/client.rs`, unknown fields ignored. |
| `agent.start {name, kind, pane_id, args, timeout_ms}` activates an idle shell pane with a canonical agent (`claude`, `codex`, `opencode`, `gemini`, `copilot`, …). Names must match `[a-z][a-z0-9_-]{0,31}`. | Pane runner = create pane → `agent.start` → `agent.prompt`. |
| `agent.prompt {target, text, wait:{until, timeout_ms}}` submits and waits atomically; returns `agent_blocked` if the agent is blocked. Status enum: `idle, working, blocked, done, unknown`. | Completion detection uses Herdr's semantic agent state. No screen parsing for completion. |
| `blocked` = Herdr recognized the agent's own approval/question UI. | Surfaces as step status `awaiting_human`; we notify and never auto-answer. |
| Plugin `[[events]]` hooks accept only: `workspace.*` (created, updated, closed, renamed, moved, reordered, focused), `worktree.created/opened/removed`, `tab.*`, `pane.created/closed/focused/moved/exited/agent_detected/agent_status_changed`. | We hook only `pane.closed` and `worktree.removed`, as **wake-ups** for an already running daemon. Agent state is followed with server-side `agent.wait`, so the high-volume `pane.agent_status_changed` is not hooked. |
| Public pane ids persist across restart (`public_pane_numbers` in the session snapshot) but PTYs/processes do not survive a cold server restart. | Our state file is authoritative for `run → step → pane → agent_session`; recovery reconciles against `pane.list`/`agent.list`. |
| `worktree.create` may `remove_dir_all` a leftover checkout directory. | We create worktrees with `git` ourselves (explicit base SHA, collision checks) and then ask Herdr to `worktree.open --path` it as a workspace. |
| `prefix+o` is the default `open_notification_target`. | We do **not** bind it. We document an opt-in `prefix+shift+o` snippet; we never edit the user's `config.toml`. |
| Herdr cannot intercept file writes or commands executed **inside** an agent process. | Enforcement boundary (§8): pre-flight only for orchestrator-owned commands; diff-based checks at step boundaries for agent actions. |

## 3. Component map

```text
                        ┌──────────────────────────────────────────────┐
 herdr-plugin.toml ───▶ │ herdr-orchestrator (single Rust binary)      │
  actions / panes /     │                                              │
  startup / events      │  cli/      clap front-end (thin client)      │
                        │  ui/       ratatui orchestrator pane          │
                        │  daemon/   single-instance engine host        │
                        │  engine/   scheduler + run driver (FSM)       │
                        │  workflow/ YAML DSL, validation, templates    │
                        │  runners/  AgentRunner: pane, headless, shell,│
                        │            fake                               │
                        │  checks/   argv command execution, capture    │
                        │  policies/ rule engine + command normalizer   │
                        │  approvals/ durable approval requests         │
                        │  audit/    JSONL + hash chain + redaction     │
                        │  git/ github/  git + gh adapters              │
                        │  herdr/    socket client + adapters           │
                        │  store/    atomic JSON, locks, migrations     │
                        │  recovery/ startup reconciliation             │
                        │  security/ path containment, env, sandbox API │
                        │  telemetry/ usage records (local only)        │
                        └──────────────────────────────────────────────┘
                                   │ socket (NDJSON)          │ argv
                                   ▼                          ▼
                              Herdr server             git, gh, claude, codex …
```

The engine depends only on traits (`HerdrApi`, `AgentRunner`, `Git`,
`GitHub`, `ExecutionSandbox`). Herdr-specific request shapes live exclusively in
`src/herdr/`, so an API change in Herdr touches one module.

## 4. Process model (the daemon decision)

Herdr plugin v1 offers no supervised long-running process, yet orchestration
must survive closing the Herdr client, the orchestrator pane and plugin
restarts. Options considered:

1. *Engine inside the TUI pane* — rejected: closing the pane kills runs.
2. *One detached process per run* — workable but complicates global limits
   (`max_parallel_runs`, `max_parallel_agents`) and queue fairness.
3. **Single detached daemon (chosen)** — `herdr-orchestrator daemon run`,
   started on demand (`daemon ensure`, ssh-agent style) by the startup hook,
   CLI commands and the TUI. Single instance guaranteed with `flock` on
   `locks/daemon.lock`. It owns the scheduler and run driver threads.

Communication is **state-first**: every mutation (new task, approval decision,
cancel, pause) is written durably to the state directory by whoever issues it,
then the daemon is *nudged* through `daemon.sock` (a Unix socket, NDJSON). The
daemon also polls the state directory every second, so a lost nudge only costs
latency, never correctness. Nothing is lost if the daemon is down: the next
daemon start picks it up.

Concurrency uses OS threads and blocking I/O rather than an async runtime. The
engine is a handful of long-lived, mostly-waiting state machines (≤ tens of
runs); threads keep the code linear and debuggable and avoid pulling in tokio.
This deviates from the brief's suggested `tokio` stack on purpose; the trait
boundaries leave room to move to async later without touching the domain model.

## 5. Domain model (`src/model.rs`)

Explicit, serde-typed entities: `Task`, `Run`, `Workflow`, `WorkflowStep`
(`Agent`, `Command`, `Approval`, `GithubPr`, `Git`, `Policy`, reserved
`Parallel`), `StepExecution`, `Variant` (a `Run` with `variant_index`),
`Artifact`, `AuditEvent`, `PolicyDecision`, `ApprovalRequest`,
`ExecutionResult`, `UsageRecord` (`measured | reported | estimated | unknown`),
`TaskStatus`, `RunStatus`, `StepStatus`.

```text
TaskStatus: queued running blocked awaiting_approval succeeded failed cancelled
RunStatus:  pending preparing running awaiting_approval blocked needs_human
            succeeded failed cancelled
StepStatus: pending starting running retrying awaiting_approval awaiting_human
            succeeded failed skipped cancelled
```

Transitions go through `RunStatus::can_transition_to` / `StepStatus::…`; an
illegal transition is a bug and returns an error instead of silently writing.
Status is never inferred from presentation.

## 6. Durable run state machine

```text
PENDING → PREPARING (worktree) → RUNNING ─┬─ step ok → next step
                                          ├─ step failed + on_failure.retry → RETRYING → target step
                                          ├─ approval step / policy ASK → AWAITING_APPROVAL
                                          ├─ agent blocked in its own UI → (step) AWAITING_HUMAN
                                          ├─ policy DENY on diff → BLOCKED
                                          └─ cancel → CANCELLED
all steps done → SUCCEEDED      unrecoverable error → FAILED
recovery ambiguity → NEEDS_HUMAN
```

Write-ahead rule: before any side-effecting action (create worktree, start
agent, send prompt, run command, commit, push, open PR) the driver persists a
`StepExecution` in `starting` with a fresh `exec_id` and the *intent*
(argv / action). Only then it acts, then persists the outcome. Recovery can
therefore always tell "never started" from "maybe happened".

Each run records `run_id, task_id, workflow_name, workflow_sha256, repo_root,
base_ref, base_sha, branch, worktree_path, head_sha, started_at, updated_at,
completed_at, initiator, status, steps[], policy_decisions[], approvals[],
artifacts[], usage[]`.

## 7. Persistence (`src/store/`)

```text
$HERDR_PLUGIN_STATE_DIR/            (fallback: $XDG_STATE_HOME/herdr-orchestrator)
├── state/
│   ├── tasks/<task_id>.json
│   ├── runs/<run_id>.json
│   ├── approvals/<approval_id>.json
│   ├── control/<run_id>.json       cancel / pause requests
│   └── scheduler.json              queue paused flag
├── audit/<run_id>.jsonl, audit/global.jsonl
├── logs/<run_id>/<exec_id>.log     raw, redacted command/agent output
├── locks/                          daemon.lock, queue.lock, worktree.lock, run-<id>.lock
├── backups/                        pre-migration copies
├── cache/
├── daemon.sock, daemon.log
```

* Every document is an envelope `{schema_version, kind, sha256, data}`; the
  hash detects truncation/corruption. Corrupt files are quarantined
  (`*.corrupt-<ts>`), reported by `doctor`, never silently discarded.
* Atomic replace: write temp in same dir → `fsync` → `rename` → `fsync(dir)`.
* `flock` guards read-modify-write cycles; locks are never held while waiting
  on agents.
* Schema migrations run through a registry `(kind, from) → fn`; the original is
  copied to `backups/` first.
* Large outputs never enter state files: state stores log paths plus a bounded
  excerpt.

JSON/JSONL beats SQLite for the MVP: human-inspectable, trivially recoverable,
append-safe audit, no native dependency. SQLite remains the planned backend if
cross-process query load grows; the `Store` API is the seam.

Config lives separately: global `$HERDR_PLUGIN_CONFIG_DIR/config.yaml`, project
`.ai/herdr-orchestrator/config.yaml`.

## 8. Security & enforcement boundaries (summary; see SECURITY_MODEL.md)

Two different trust situations, never conflated:

**A. Orchestrator-controlled actions** — workflow `command` steps, git
commit/push, PR creation, agent launch arguments. These pass *pre-flight*:
`normalize → policy → ALLOW | REQUIRE_APPROVAL | DENY`, all audited, before
execution. Commands are argv arrays executed directly; `shell: true` is an
explicit opt-in that is visible in the UI, audited, and evaluated against both
the full script and each `&&`/`||`/`;`/`|` segment.

**B. Actions inside agent processes** (Claude/Codex running their own tools in a
Herdr pane). We **cannot** intercept these in real time. We can:
configure agent permission modes at launch (policy-checked), confine cwd to
the run's worktree, sample the pane's foreground process (`pane.process_info`,
best-effort, labelled *sampled*), and — the enforceable part — inspect the
worktree diff at every step boundary and before approval/PR. A denied path
blocks the run; a require-approval path pauses it. This is **not a sandbox**
and is documented as such. `ExecutionSandbox` is the seam for future OS-level
isolation (bubblewrap, containers, macOS profiles); MVP ships `none`.

Paths are canonicalized and contained within the worktree before evaluation;
symlinks that escape the worktree are themselves reported as violations.
Child processes receive an explicit environment built from an allowlist
(plus agent-auth variables the user opts into); environments are never logged.

## 9. Workflow DSL (see WORKFLOWS.md)

YAML, `version: 1`, steps of type `agent | command | approval | github_pr |
git | policy`, with `on_failure.retry_step/max_attempts/feedback`, per-step
`runner`, `skill`, `timeout`, `when` (only `previous_succeeded`/`always`), and
constrained templating: `{{task}}`, `{{run.id}}`, `{{repo.root}}`,
`{{worktree.path}}`, `{{branch}}`, `{{previous.output}}`,
`{{step.<id>.output}}`, `{{feedback}}`, `{{output_file}}`. No expressions, no
filters, no shell, and values are never re-expanded. Trust rules for `command`
steps: templates never appear in argv[0]; *untrusted* variables (`task`,
`task_title`, `previous.output`, `feedback`, `step.*.output`) are rejected in
argv and env (argument-injection risk); `shell: true` scripts may use no
templates at all and read `HERDR_ORCH_*` environment variables instead.
`type: check` steps can name a check (`tests`, `lint`, `security`) resolved
from project config or auto-detection at run time. Workflow content is hashed
and snapshotted into the run for auditability and recovery.

## 10. Runners (see RUNNERS.md)

```rust
trait AgentRunner {
    fn capabilities(&self) -> RunnerCapabilities;
    fn start(&self, ctx) -> Result<AgentHandle>;           // idempotent per exec_id
    fn send(&self, handle, prompt) -> Result<()>;
    fn wait(&self, handle, deadline, cancel) -> Result<AgentOutcome>;
    fn interrupt(&self, handle) -> Result<()>;
    fn collect_output(&self, handle) -> Result<AgentOutput>; // result file / fallback
    fn collect_usage(&self, handle) -> UsageRecord;
    fn recover(&self, persisted) -> Result<RecoveryAssessment>;
}
```

* `HerdrPaneRunner` (kinds `claude`, `codex`, `opencode`, extensible to any
  Herdr agent kind): new tab in the run workspace → `pane.rename`
  (`#<short> implement · codex`) → `agent.start` → `agent.prompt` with wait →
  loop through `blocked` (notify human) until `idle/done`. Retries with
  feedback go to the **same live agent**, which keeps its context.
* Structured output: the prompt asks the agent to write JSON to
  `{{output_file}}` (inside `<worktree>/.herdr-orchestrator/out/`, git-excluded).
  The file is validated; if missing/invalid the raw pane tail
  (`pane.read recent-unwrapped`) is kept and marked `parse_failed`.
* `HeadlessRunner` (claude `-p --output-format json`, codex `exec --json`):
  no pane, but provider-reported usage/cost. Used when Herdr is unavailable
  (CI, headless CLI).
* `ShellRunner`: arbitrary argv agent (aider, custom scripts), prompt on stdin.
* `FakeRunner` (`fake:success|fail|timeout|review-findings|fix-on-retry|
  touch-secret|touch-migration`): deterministic, used by `cargo test` and dry
  demos. No accounts needed.

The engine never contains provider-specific logic; provider details live in a
`RunnerProfile` (kind, launch args, permission-mode args, headless argv).

## 11. Policy engine (see POLICY_ENGINE.md)

Rules match on `actions`, `paths` (globset), `commands` (normalized wildcard),
`runners`, `branches`, `steps`, `repos`, with optional thresholds
(`min_lines_changed`, `min_files`). Decision lattice: `deny > require_approval
> allow`; the most restrictive matching rule wins, and every matching rule id is
recorded. Unmatched evaluations fall to `default_decision` (default policy:
`allow` for ordinary work, with a conservative rule set). Command normalization
strips absolute argv[0] paths, leading env assignments, `env`/`sudo`/`nohup`
wrappers and git global options (`-C`, `-c`, `--git-dir`), and unwraps
`sh -c`/`bash -c`, so `/usr/bin/git -C x push -f` matches `git push*-f*`.

## 12. Approvals, audit, telemetry

* Approvals are durable documents with full context (task, step, reason,
  agent, repo, branch, changed files, diff stat, checks, policy reasons, exact
  action). Decisions: approve-once, deny, cancel run. Persistent policy
  weakening is intentionally not in MVP.
* Audit: one JSONL file per run (+ global), events hash-chained
  `hash = sha256(canonical(event \ {hash}) ‖ previous_hash)`; `audit verify`
  recomputes the chain. Tamper-*evident*, not signed; the event schema reserves
  `signature` for a future signing key. All strings pass through the redactor.
* Usage: `UsageRecord{input,output,cached tokens, cost_usd, source}` where
  `source ∈ measured | reported | estimated | unknown`. Pane agents report
  `unknown` in MVP; headless runners report provider values. No hard cost
  enforcement is claimed.
* No external telemetry, analytics or crash upload. Ever by default.

## 13. Recovery (see RECOVERY.md)

On daemon start: load runs in non-terminal states → reconcile each step
execution against Herdr (`pane.list`, `agent.list`), the process table (for
direct commands, recorded pid + start time), git (`worktree list`, HEAD,
dirty state) and GitHub (`gh pr list --head`) → classify
`recoverable | needs_human | stale | completed_externally | failed`. Only
provably safe continuations resume automatically (waiting on a still-live
agent, re-evaluating a pending approval). Anything ambiguous with side effects
becomes `needs_human`. Nothing is re-executed blindly. Idempotency keys
(`exec_id`, `approval_id`, branch name, PR head) prevent duplicate worktrees,
agent launches, commits and PRs.

## 14. Git & GitHub

One worktree + branch per run: `<repo>/.herdr-orchestrator/worktrees/<task>-<slug>[-v<n>]`,
branch `herdr/<task>-<slug>[-v<n>]`. Pre-checks: repo discovery, base SHA
capture, branch-name validation (`git check-ref-format`), collision checks,
containment. `.herdr-orchestrator/` is added to the repo's `.git/info/exclude`
(idempotent, audited). Never force-reset, never force-push, never remove a
dirty worktree without approval. GitHub via `gh`: read issue, create **draft**
PR (policy: require approval), record URL. Nothing auto-merges or deploys.

## 15. UI

`[[panes]] id = "dashboard"` runs `herdr-orchestrator ui`: a ratatui TUI that
reads state (no engine inside). Views: dashboard (running/queued/finished runs
with step timelines), run detail (steps, files changed, +/-, policy summary,
usage), approvals (full context, approve/deny/cancel), diff viewer, new-task
form (repo defaults from `HERDR_PLUGIN_CONTEXT_JSON`). Focus-agent-pane uses
`agent.focus`/`pane.focus`. A popup action `new-task` opens the form directly.

## 16. Configuration precedence

```text
built-in defaults → global config → project config → workflow defaults
→ task options → CLI flags
```

Each layer is a partial struct; merging is field-wise and deterministic
(later wins; lists replace; policy files are *concatenated* so a project can
only add rules, never silently drop global denies — `policy.replace_global:
true` is an explicit, audited escape hatch).

## 17. Testing strategy

* Unit: workflow parse/validate/template, FSM transitions, retry accounting,
  policy matching + normalization, path containment, config merge, audit chain
  and tamper detection, redaction, git naming, usage aggregation.
* Integration (`tests/`): real `git` in temp repos, `MockHerdr` implementing
  `HerdrApi`, fake runners, fake `gh`; scenarios: success, retry→success,
  retry exhausted, approval approve/deny, policy deny/ask, runner crash, pane
  disappears, restart recovery, cancel, parallel runs, variants. Plus an NDJSON
  mock socket server exercising the real Herdr client.
* E2E (opt-in, `HERDR_ORCH_E2E=1`): against a *named* Herdr session, never the
  user's default session.

## 18. Lessons from real-Herdr E2E (implemented)

* `agent.start` right after `tab.create` returns `agent_pane_busy` until the
  new shell owns the foreground → bounded retry.
* After a human answers an agent's dialog (e.g. Claude's folder-trust
  prompt) Herdr reports `idle` before the agent's input box accepts text; a
  prompt sent then is swallowed → wait for `interactive_ready`, settle, and
  re-confirm `idle`. A stalled prompt is never resent blindly (`needs_human`).
* Unix socket paths are limited (~104 bytes on macOS) → long state dirs put
  `daemon.sock` in a private `/tmp/herdr-orch-<uid>/` (0700, owner-checked).
* Daemon shutdown neither cancels nor joins runs; recovery reattaches.

## 19. Explicit MVP cuts (documented, typed where useful)

* `parallel` step type: parsed & rejected with a clear message; variants cover
  the tournament use case at the run level.
* Visible command panes (running `command` steps inside a Herdr pane via a
  wrapper) — roadmap; MVP captures commands directly and shows logs in the UI.
* Sandbox backends other than `none`; signed audit; persistent approval rules;
  cost enforcement (limits are advisory and only apply to reported usage).
* Policy actions `network` and `env_mutation` are reserved: the orchestrator
  cannot observe them for agent processes; network *commands* are covered by
  the `network` command tag instead.
