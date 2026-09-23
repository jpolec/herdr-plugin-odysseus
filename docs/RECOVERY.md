# Durability and recovery

Closing the Herdr client, the orchestrator pane or the daemon must never lose
orchestration state, and a restart must never re-run an ambiguous side effect
blindly.

## Durable state

```text
$STATE/                                  (HERDR_PLUGIN_STATE_DIR or HERDR_ORCH_STATE_DIR)
├── state/tasks/<n>.json                 Task
├── state/runs/<run_id>.json             Run: steps, bindings, git state, cursor
├── state/approvals/<approval_id>.json   ApprovalRequest + decision
├── state/control/<run_id>.json          cancel / pause / resume requests
├── state/scheduler.json                 queue paused flag
├── state/task_counter
├── audit/<run_id>.jsonl, audit/global.jsonl
├── logs/<run_id>/<exec_id>.log          redacted raw output
├── locks/  backups/  cache/
└── daemon.sock, daemon.pid, daemon.log
```

Every document is an envelope `{schema_version, kind, sha256, data}` written
atomically: temp file, fsync, rename, fsync of the directory.

## Write-ahead rule

Before every side effect the run driver persists a `StepExecution` with a
fresh `exec_id`, status `starting` and its `intent`. Only then does it act, and
it persists the outcome afterwards. Two agent-specific markers are persisted
at the moment they become true:

- `agent` binding (pane, agent name, workspace), recorded as soon as the pane
  exists and again after `agent.start`;
- `prompt_sent = true`, set **before** the prompt is submitted, so a crash
  after that point is read as "maybe sent".

Worktree names and the base SHA are also persisted before `git worktree add`.

## On daemon start

`daemon run` takes the single-instance lock, then calls
`recovery::recover_all` (`src/recovery/mod.rs`) before scheduling anything.
The CLI `--foreground` mode does the same. Recovery handles every run in
`preparing`, `running` or `awaiting_approval`, except one whose per-run lock is
held by a live process, which it skips.

1. Audit `recovery_started`.
2. Classify the run (table below).
3. Apply the classification:
   - **recoverable / stale**: status becomes `pending` with `recovered = true`.
   - **completed_externally**: the in-flight step is marked succeeded, the cursor advances, status becomes `pending`.
   - **needs_human**: status becomes `needs_human` and you get a Herdr notification.
   - **failed**: open steps are marked failed and the run becomes `failed`.
4. Audit `recovery_completed` with the classification and reason.

When the scheduler picks a recovered run back up, the driver finds the step
execution that is still open and resumes it rather than starting a new one.

### Classification

| Situation | Result |
| --- | --- |
| Worktree recorded but missing on disk | **failed** |
| No step in flight, `preparing` | recoverable (worktree creation is idempotent) |
| No step in flight, cursor past the last step | stale (completes) |
| No step in flight otherwise | recoverable (next step) |
| Open step has a **pending approval** | recoverable (re-wait on the same `approval_id`) |
| Agent step, no binding yet | recoverable (launch) |
| Agent alive, `prompt_sent` | recoverable: **reattach and wait**, prompt not resent |
| Agent alive, not prompted | recoverable: send the prompt to that agent |
| Agent gone, not prompted | recoverable: relaunch |
| Agent gone, `prompt_sent` | **needs_human**: partial work may be in the worktree |
| Agent state can't be verified (Herdr unreachable, runner unknown) | **needs_human** |
| `check` step interrupted | recoverable: orphaned process group is terminated, check re-runs (checks are read-only by contract) |
| `command` step spawned and interrupted | **needs_human**: orphan terminated; the command may have had side effects |
| `command` step `starting` with no process recorded | recoverable (never spawned) |
| `github_pr` step | checks `gh pr list --head <branch>`: PR exists → **completed_externally**; none → recoverable (the step re-checks before creating); query error / no `gh` → needs_human |
| `git`, `policy`, `approval` steps | recoverable (idempotent: no force, empty commit is a no-op) |

## Idempotency keys

- `exec_id`: one execution record per attempt. A resumed attempt keeps its id and its output file `.herdr-orchestrator/out/<exec_id>.json`.
- `approval_id`: a resumed approval step reuses its pending request instead of creating a new one. Decisions are one-shot, so a second approve or deny is refused.
- Branch name: `git::add_worktree` returns `Existing` for the exact worktree/branch pair, and refuses any other collision. It never reuses or resets a branch it didn't create for this run.
- PR head: before creating a PR, the step checks `gh pr list --head <branch>`.
- Agent name `o<task><variant>-<step>-<4hex>` is unique per launch. Retries reuse the live agent instead of launching another.

## Human retry

`herdr-orchestrator run retry <run> [--from-step S]` applies to runs that are
`failed`, `cancelled`, `blocked` or `needs_human`. It records a durable resume
request, and the scheduler then:

- marks any still-open executions `cancelled` ("superseded by human retry");
- sets the cursor to `S`, or leaves it at the step that stopped;
- resets retry counters for that step and every later step;
- clears the cancel flag, sets the run back to `pending` and audits `retry_started` (human).

The worktree, branch and commits are kept. Typical use: fix something by hand
in the worktree, then retry the failed check.

## Cancel and pause

- `run cancel` writes `cancel_requested` durably. A running driver sees it within one poll through its control watcher, interrupts the agent (`esc` for claude/codex, `ctrl+c` otherwise) or terminates the command's process group (SIGTERM, then SIGKILL after 5 s), cancels pending approvals and ends `cancelled`. Queued, blocked and `needs_human` runs are cancelled directly by the scheduler. Worktrees are kept.
- `run pause` takes effect at the next step boundary. `run resume` continues. `queue pause` stops new runs from starting while running runs continue.
- `daemon stop` does **not** cancel runs. The daemon exits and the next start recovers them.

## Corruption, migrations, backups

- A document whose JSON fails to parse, or whose checksum doesn't match, is renamed to `*.corrupt-<timestamp>` and reported as an error. `doctor` lists quarantined files. They are never silently deleted.
- An older `schema_version` is migrated step by step through the registry in `src/store/migrate.rs`, after the original is copied to `backups/`. A newer version is refused with "upgrade herdr-orchestrator".
- The audit log is append-only JSONL. `audit verify` detects edits, deletions and reordering.

## Locks

| Lock | Purpose |
| --- | --- |
| `locks/daemon.lock` | one daemon per state directory (`flock`, non-blocking) |
| `locks/run-<id>.lock` | exactly one driver per run; a contender steps aside without touching the run |
| `locks/queue.lock` | claiming queued tasks (re-checked under the lock) |
| `locks/task-<n>`, `approval-<id>`, `control-<id>`, `counter`, `audit-<run>` | short read-modify-write sections |

Locks are never held while waiting on agents or humans.

## Foreground and daemon together

`--foreground` runs a scheduler in the CLI process. If a daemon is also
running, task claiming happens under `queue.lock` and each run is driven by
whichever process first takes its run lock. Neither process can create
duplicate runs or double-drive one.
