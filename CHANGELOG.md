# Changelog

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
