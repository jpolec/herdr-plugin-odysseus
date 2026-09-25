# Changelog

## [Unreleased]

### Fixed
- **`repos:` now scopes file rules.** The run's repository root was set only on worktree-creation and command subjects, so a rule combining `paths` (or `added_lines`) with `repos` never matched at the diff gate, in the Claude `PreToolUse` hook, on commits or on PR creation. Every run subject now carries it, and `policy check` fills it from `--repo` / the Herdr context / the cwd.

## [0.4.0] - 2026-09-25

Project memory: agents start with what earlier runs in the same repository did, decided and got wrong.

### Added
- **Prior work in agent prompts** (`memory.history`, default on). Before an implementing, contract or planning agent starts, the orchestrator ranks this repository's history and adds a short, dated "Prior work in this repository" section (at most `memory.max_records` records within `memory.max_chars`): changes with their files and PRs, failed attempts with the reason, critical/high review findings, unmet acceptance criteria, approved contracts, ADR decisions of accepted epics, humans' reasons for denying an approval, and notes. Ranking: overlap with the task's scope and the paths it names first, then rare shared words, recency and trust (facts and humans over agents' opinions); a record needs file or word overlap to appear. Reviewers never get it, so their judgement stays independent. Each injection is audited (`memory_injected`, with every record and why it was chosen).
- **Active work**: the same section lists runs working in the repository right now and where (`memory.active`).
- **Notes for agents.** `herdr-orchestrator note add "…" [--scope 'src/fx/**']`, `note list`, `note rm`. Agents can leave `notes_for_others` in their result file (stored with the files they changed as scope). Notes reach agents whose work touches the scope.
- `herdr-orchestrator history --task N` shows exactly the section a task's agent gets, with the score and reason for each record; `history "free text or paths"` searches the history.
- **Measure it**: `eval run --runners claude --arms history,no-history` runs every case once per arm; `eval report` shows one row per runner and arm. Replays never see the recorded solution: history is cut at the moment the original run started and the original task is excluded.

## [0.3.1] - 2026-09-25

### Added
- **Run state in Herdr's sidebar.** Each run's workspace gets a sidebar token (`$orch` by default, `herdr.sidebar_token`) that says what the run does or needs: `⏳ approve: contract`, `→ implement · claude`, `❓ implement asks you`, `⚠ stuck: implement`, `⛔ blocked: …`, `✗ failed: …`, `✓ done · 3.6k tok`. Reported through `workspace.report_metadata` only when it changes. To show it, add `[{ token = "$orch" }]` as a row of `[ui.sidebar.spaces]` in your Herdr config (the plugin never edits it). Orchestrator approvals are now visible without opening the popup.

## [0.3.0] - 2026-09-25

Contract-first delivery, a morning inbox for unattended work, a benchmark from your own repository, and integrations with GitHub issues and Sentry.

### Added
- **Contract-first delivery** (workflow `contract-first`, skill `contract-writing`). An agent first writes the tests for the task (`output: contract`: the test files, the command that runs them, and which test covers which acceptance criterion). The contract's check must **fail** on the current code — a contract that already passes goes back to its writer. You approve the contract, seeing its files, check, criteria map and the failing output; from then on it is **locked**: any change to a contract file is denied at the diff gate (content hash) and, for Claude, refused by the `PreToolUse` hook before it happens. `check` steps with `contract: true` run the locked check. If you edit the tests before approving, the lock takes your version and the receipt says it was amended. The PR body carries a receipt (contract hash, approver, audit head); `herdr-orchestrator receipt verify <run|pr-url> [--at rev]` re-checks it from git and the audit chain.
- **Your repository's own benchmark.** `eval record <run>` turns a succeeded contract-first run into a case (base commit, task, approved contract). `eval run --runners claude,codex,… [--cases …|--last N]` replays cases on several agents from the same base with the same contract seeded and locked (workflow `eval-task`); without `--yes` it only prints a cost estimate from your history. `eval report` ranks runners by cases passed, attempts, tokens and time. `eval list`.
- **Morning inbox.** `herdr-orchestrator inbox [--since 24h]` (and `[i]` in the pane) lists everything that needs you, most urgent first: agents asking or stuck, approvals, stopped runs, plans to decide, finished work. Approvals and finished work carry a risk level (`low`/`medium`/`high`) with the reasons it was derived from — sensitive rules that fired, diff size, review findings, acceptance results, retries, the watchdog, an approved contract. `approval batch --max-risk low` (or `[A]`) approves all "ship it?" steps up to that risk; policy questions are never approved in bulk.
- **Night shift.** `shift start --until 07:00 --budget 2000000` runs the queue until the deadline or token budget, then pauses it and notifies you that the inbox is ready; `shift status`, `shift stop`.
- **Watchdog for agents in panes** (`guard.watchdog`): an agent that is busy without progress — screen unchanged or the same error coming back, while no file changes and tokens are being used (or usage is unknown) — is handed to you as `awaiting_human` with the reason, shown on the dashboard as "looks stuck". It never fails a step and re-arms when the agent makes progress.
- **Learning from reviews.** `herdr-orchestrator learn [--all] [--list]` collects recurring review findings, unmet acceptance criteria, PR review comments and guardrail hits, and queues a task for an agent to turn them into rules in `AGENTS.md` / `CLAUDE.md` / `.ai/skills/`. Those files need your approval (default policy), so nothing changes silently. `guard.learn_reminder` (default 20) notifies at most daily when findings pile up.
- **GitHub tracker** (`github.tracker`). `tracker sync`: a milestone per epic, an issue per accepted epic task (description, acceptance checklist, manual checks), status comments posted once per change (started, done with PR or branch and tokens, failed, blocked), `Closes #N` in the PR body; optionally the issue is added to a Projects board (`github.tracker.project: owner/number`, needs `gh auth refresh -s project`). `tracker import [--label agent-ready] [--yes]` queues tasks from labelled issues. `github.tracker.auto_sync` syncs while the engine runs.
- **Production errors from Sentry.** `incidents list`, `incidents show <id>`, `incidents task <id|SHORT-ID>` turn an unresolved Sentry issue into a task that reproduces the error with a failing test first and then fixes it (default workflow `contract-first`); the PR links the Sentry issue. Configure `sentry.org`, `sentry.project`; token from `SENTRY_AUTH_TOKEN`.
- **Parallel work without collisions.** `scheduler.avoid_conflicts` (default on): a task with a `scope` waits while an active run's scope or changed files overlap it ("may conflict with #12"). `run update <run> [--onto main]` (or `[u]`) merges the moved base into a finished run's branch — never a rebase or force push; a clean merge is re-tested (`update-verify`), conflicts go to an agent (`update-resolve`), and the result is pushed after your approval.

### Changed
- `check` steps accept `contract: true` (run the locked contract's check) and prompts can use `{{contract}}` (untrusted, prompt-only: the locked files and check).
- Follow-ups created by `run update` diff and check policy against the merged base, so changes that came from the base are not treated as the branch's own.

### Security
- Contract files are locked per run: DENY at the diff gate on any content change, and in real time for Claude's file tools. Shell edits by other agents are caught by the hash check at the next diff gate, before anything is committed or pushed.
- Sentry data is minimised: only the issue title, culprit, counts, first/last seen, release/environment tags and the exception type, message (redacted) and in-app stack frames reach the prompt — never requests, users, breadcrumbs or contexts. The token reaches `curl` on stdin, never in its arguments.
- Batch approval never covers policy questions; GitHub issue text and Sentry messages are untrusted input and only ever reach prompts.

## [0.2.0] - 2026-09-25

Guardrails against agents weakening their own checks, token usage for agents in panes, and ADR-driven epics.

### Added
- **Token usage of pane agents.** Claude and Codex keep local session logs (`~/.claude/projects`, `~/.codex/sessions`, honouring `CLAUDE_CONFIG_DIR` / `CODEX_HOME`). After each agent step the orchestrator reads the step's time window from them: `reported` when Herdr gave the agent's native session id, `estimated` when the log was found by working directory. Read-only and local; cost stays unknown (the logs carry no price). Switch off with `usage.session_logs: false`.
- Tokens per task and step: `task list`, `task show`, `run list`, `run show` (TOKENS column), the dashboard (per run and per step) and a `Tokens` total in the footer. New `herdr-orchestrator usage [--task N] [--all]` prints input/output/cached tokens per task.
- **Epics: from an ADR to verified work.** `adr list` finds ADRs (`epic.adr_dirs`; Nygard, MADR and front-matter status). `epic create --from docs/adr/0007-x.md` runs a read-only planning agent (built-in `epic-plan` workflow, `output: plan`); an invalid plan goes back to the planner with the errors, a planner that changes files is stopped. `epic show` lists tasks, dependencies, acceptance criteria, verification commands and open questions; `epic accept [--only T1,T3]`, `epic reject [--only …]`, `epic replan --feedback "…"`, `epic edit` ($EDITOR, validated). Accepted tasks are queued with their dependencies, scope, acceptance criteria and manual checks; the plan's verification commands become check steps (still policy-checked). `epic verify` runs a read-only conformance review against the ADR (`epic-conformance`); gaps become proposed follow-ups `F1…`, accepted like the plan. ADR changes after planning are shown as drift.
- Task dependencies: `epic.dependency_mode: merged` (default; a dependency counts once its commits are in the base branch, checked locally with `git merge-base --is-ancestor`) or `stacked` (branch from the one unmerged dependency; the PR targets its branch). A failed or cancelled dependency blocks its dependents; `task unblock <id>` overrides. Queued tasks show why they wait.
- `output: acceptance` with the `acceptance-review` skill: each criterion is `met`, `not_met` or `unverifiable` with evidence; with `gate: true` unmet criteria go back to the implementer. `{{acceptance}}` (untrusted, prompt-only) holds the task's numbered criteria. Approvals and the PR body show the criteria table and the manual checklist.
- Built-in workflows `epic-task` (implement → tests → gated acceptance review → approval → draft PR) and `dual-review` (Codex implements; Claude and Codex review one after the other, both gated). Skills `adr-planning`, `acceptance-review`, `adr-conformance`.
- **PR follow-ups.** `run followup <run> [--note …] [--runner …]` (or `F` in the pane) turns failing CI checks and review comments of the run's PR into a task on the same branch and worktree; new commits are pushed to the existing PR (push still needs approval). `github.watch_prs: true` checks open PRs of recent runs every `github.watch_interval` and only notifies; the daemon stays up while it is on.
- `herdr-orchestrator gc [--yes] [--check-prs] [--failed --days N]`: removes worktrees of finished runs that are merged, closed or superseded by the selected variant. Never a dirty worktree, never a branch.
- `herdr-orchestrator stats`: per implementing runner and workflow — runs, success rate, attempts, first-review approvals, average tokens and minutes. Local data only.
- TUI: Epics screen (`e`; `y` accept open tasks, `n` reject, `g` re-plan, `v` verify against the ADR), `F` PR follow-up, acceptance and manual checks on the approval screen.
- `task create --scope 'src/webhooks/**'` (repeatable): changes outside the declared scope need approval (`task-scope`).

### Changed
- `limits.max_tokens` and `limits.max_cost_usd` are enforced: when reported or log-read usage of a run exceeds them, the run asks once whether to continue (deny → `blocked`). Unknown usage is still never treated as under budget.
- Claude agents start with `--settings <worktree>/.herdr-orchestrator/claude-settings.json` (see Security).
- Follow-up and epic tasks keep the original task's runners; nothing falls back to the default runner silently.

### Fixed
- `config init` also writes the project `policy.yaml` its sample config refers to; before, the first run failed with "No such file or directory".

### Security
- New default rules (require approval): `approve-agent-instructions-and-orchestrator-config` (`.ai/herdr-orchestrator/**`, `.ai/skills/**`, `.claude/**`, `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.codex/**`, `.cursor/**`, `.mcp.json`, …), `approve-test-deletion`, `approve-test-runner-config` (pytest/jest/vitest/coverage settings) and `approve-disabled-tests`.
- New policy criterion `added_lines`: regular expressions matched against the lines a change adds (diff gate only, read only when a rule uses it). `approve-disabled-tests` uses it to catch added `#[ignore]`, `it.skip`/`.only`, `xit`, `@pytest.mark.skip`/`xfail`, `t.Skip`, `@Disabled`, … — re-enabling a test is never flagged.
- `guard.test_only_retry` (default on): after a failed check sends feedback to an agent, a new attempt that changed only tests or test configuration (`guard.test_paths`) needs approval (`guard-test-only-retry`).
- Real-time policy for Claude: a `PreToolUse` hook (`guard.claude_hook`, default on) checks `Bash`, `Write`, `Edit`, `MultiEdit`, `NotebookEdit` and `Read` calls against the effective policy before they run; a DENY blocks the call and tells the agent why. Approval-level decisions are left to Claude's own permission mode and the diff gate, so nothing is asked twice. The settings file lives in the git-excluded orchestrator directory, never in the project's `.claude/`.

## [0.1.9] - 2026-09-23

Fixes from an external code review, each with a regression test that fails on 0.1.8.

### Fixed
- Resuming an execution after a restart deleted its result file, so work an agent finished while the engine was down was lost (a written `approved` review became a failed run). Only a fresh execution clears its result file.
- Reattaching to an agent after a restart skipped the completion check and took a momentary `idle` as done. Fresh prompts and reattaches now share one completion check (result file, or idle for a whole settle window).
- The completion check reported "done" on cancellation, on the step deadline and when the agent vanished. These are now a cancel, a timeout and a lost agent; errors talking to Herdr propagate.
- `rust-version` said 1.82 but dependencies (ratatui 0.30, time) need 1.88. Set to 1.88, and CI now builds on exactly that toolchain.

## [0.1.8] - 2026-09-23

### Fixed
- A step could be committed while the agent was still working. Claude briefly reports `idle` between tool calls and permission prompts; run #5 was committed after 2 minutes and the agent kept editing for 30 more. The result file is now the completion signal: `idle` without it is accepted only after the agent stays idle for a whole `herdr.settle_window`.

### Added
- `docs/ROADMAP.md`: ADR-driven epics (plan → accepted tasks → verified outcome), ideas from Cezar, real-time Claude policy via `PreToolUse` hooks. Written by the orchestrator in run #4.

## [0.1.7] - 2026-09-23

### Changed
- `herdr.close_panes_on_success` now defaults to `true`: when a run succeeds, its agent panes and its Herdr workspace are closed so no agent stays alive holding the task's instructions (an agent left open after a run pushed to `main` by itself). Worktree files and the branch stay. Failed, blocked and cancelled runs keep their panes for inspection.

### Added
- `herdr-orchestrator run close <run> | --finished`: close the Herdr panes and workspace of finished runs.
- CI on GitHub Actions (macOS + Linux: build `--locked`, tests, clippy) — written by the orchestrator itself in its first real run.

## [0.1.6] - 2026-09-23

### Fixed
- Reading agent panes always failed: the Herdr API expects `recent_unwrapped`, not the CLI spelling `recent-unwrapped`. The "agent is asking" view showed "not reachable" and agent transcripts in logs were empty. The agent view now reads the visible screen.

### Added
- Every request the Herdr client sends in tests is checked against Herdr 0.9.0's published API schema (`tests/fixtures/herdr-api-0.9.0.schema.json`): method names, parameter names and enum values.

## [0.1.5] - 2026-09-23

### Added
- See and answer agent questions from the orchestrator popup: runs whose agent is waiting show "is asking you something"; `enter` opens a live view of the agent's pane and forwards answer keys (digits, y/n/a, arrows, enter, tab). Answers are recorded in the audit log (`human_input_sent`).
- "What now" hints for failed, needs-human, blocked and awaiting-approval runs; an "Agents asking" counter in the footer.

### Fixed
- Pasting multi-line text into the new-task form turned line breaks into the letter `j`; the form now uses bracketed paste and treats ctrl+j as a newline.
- Agents are told to write their result file with their file tool rather than a shell command, avoiding needless permission prompts.

## [0.1.4] - 2026-09-23

### Fixed
- `Cargo.lock` was not updated in 0.1.3, so `cargo build --locked` (and therefore `herdr plugin install --ref v0.1.3`) failed. Use 0.1.4.

## [0.1.3] - 2026-09-23

### Fixed
- Right after a prompt, Herdr can still report the agent as idle before it has started; the review of the first real run was dropped this way. A "finished" report without a result file is now double-checked for `herdr.settle_window` (default 30s): a result file means done, renewed activity means keep waiting.

## [0.1.2] - 2026-09-23

Findings from the first real run (the plugin adding CI to its own repository).

### Fixed
- A prompt typed while an agent was still starting (update banner, MCP servers) was lost and the step "succeeded" with no changes. The runner now waits until the agent is stably idle before the first prompt, sends one reminder if the agent settles without its result file, and fails the step if there is still no result and no change.
- An approved file was asked about again after it was committed (its git status changed from untracked to added). Approvals now remember the file's content hash.
- `plan` showed headless mode even when Herdr was available.

### Changed
- The dashboard opens as a 92% popup (like lazygit/btop); `f` on a run closes it and jumps to the agent's pane.
- README: "Herdr alone vs. with herdr-orchestrator" comparison.

## [0.1.1] - 2026-09-23

### Added
- `install-cli` action: opt-in symlink of `herdr-orchestrator` into `~/.local/bin` (never overwrites a real file).
- README rewritten for first-time users (quick start, FAQ, illustrations in `assets/`).

## 0.1.0 — 2026-09-23

First MVP release. Targets Herdr ≥ 0.9.0 on macOS and Linux.

### Added

- Herdr plugin manifest:
  - build step: `cargo build --release --locked`
  - startup hook that starts the daemon and runs recovery
  - actions `open`, `new-task`, `approvals`
  - wake-up hooks on `pane.closed` and `worktree.removed`
  - dashboard and new-task panes
- Durable engine:
  - single-instance daemon, run driver with explicit state machines, write-ahead persistence
  - FIFO queue with `max_parallel_runs` and `max_parallel_agents`, pause/resume, cancel, human retry (`--from-step`)
- One git worktree and branch per run. Repository hooks are disabled for orchestrator commits, and there is no force-reset or force-push.
- Workflow DSL: `agent`, `command`, `check` (named `tests`/`lint`/`security` with auto-detection), `approval`, `policy`, `git`, `github_pr`, `on_failure` retries with bounded feedback, and constrained templates. Untrusted values are never allowed in command lines.
- Built-in workflows `quick-task`, `implement-review`, `secure-change`, `variant-review`, and skills `implementation`, `code-review`, `security-review`.
- Runners:
  - interactive agents in Herdr panes (`claude`, `codex`, `opencode`, any Herdr agent kind), with live-agent reuse on retry
  - headless (`claude -p`, `codex exec`) with reported usage
  - `shell` for custom agents
  - deterministic `fake-*` runners
- Policy engine:
  - ALLOW / REQUIRE_APPROVAL / DENY, most restrictive wins
  - command normalization and semantic tags
  - diff-based gate with symlink and containment checks
  - conservative default policy
- Approvals with full context, one-shot decisions, and an approval step that covers the next step's gated actions.
- Hash-chained, redacted JSONL audit trail with `audit verify`.
- Recovery classification (`recoverable`, `needs_human`, `completed_externally`, `stale`, `failed`) with idempotency checks for worktrees, prompts and PRs.
- Variants (tournament mode), `task compare`, explicit `task select`, `run pr`.
- CLI (task/run/approval/workflow/policy/audit/queue/daemon/config/runners/skills/plan/doctor), `--dry-run`, `--json`, and a ratatui TUI.
- Local-only usage accounting labelled `measured / reported / estimated / unknown`. No telemetry.

### Fixed during E2E against a real Herdr session

- `agent.start` right after `tab.create` failed with `agent_pane_busy`. It is now retried until the shell is ready.
- A prompt sent right after an agent's trust dialog could be swallowed. The runner now waits for `interactive_ready` and a settle period.
