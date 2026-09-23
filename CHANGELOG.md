# Changelog

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
