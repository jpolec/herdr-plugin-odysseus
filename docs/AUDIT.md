# Audit trail

Every orchestrated action is recorded in append-only JSONL files with
optional hash chaining. Source: `src/audit/mod.rs`, `src/audit/redact.rs`.

## Files

```text
$HERDR_PLUGIN_STATE_DIR/audit/
├── global.jsonl          task intake, queue and daemon events
└── <run_id>.jsonl        everything that happened in one run
```

Each append takes an exclusive `flock` on a per-log lock file, reads the
last event's `seq` and `hash`, writes one line and `fsync`s it, so the CLI,
TUI and daemon can append concurrently. Files are created with mode `0600`.

**Retention:** the tool never rotates, truncates or deletes audit files.
Archive or remove them yourself; removing lines breaks verification of that
file by design.

## Event format

```json
{
  "event_id": "ev-3f9a1c0b7d2e",
  "seq": 12,
  "timestamp": "2026-09-23T10:02:19.123456789Z",
  "run_id": "run-4a0a43ea0b01",
  "task_id": "2",
  "step_id": "implement",
  "actor": {"type": "human", "user": "jakub"},
  "event": "approval_granted",
  "data": {"approval_id": "ap-e56858347be5", "note": "migration reviewed"},
  "previous_hash": "9c1e…",
  "hash": "51ab…"
}
```

| Field | Meaning |
| --- | --- |
| `event_id` | random id |
| `seq` | 0-based position in this file |
| `timestamp` | UTC, RFC 3339 |
| `run_id`, `task_id`, `step_id` | scope (omitted when not applicable) |
| `actor` | `{type: orchestrator \| human \| agent \| policy, runner?, pane_id?, user?}` |
| `event` | event name (catalogue below) |
| `data` | event payload, redacted |
| `previous_hash`, `hash` | chain links (see below) |
| `signature` | reserved for a future signing key; omitted today |

Human actors carry the OS user (`$USER`) of whoever made the decision.

## Event catalogue

Global log:

| Event | When |
| --- | --- |
| `task_created` | a task is accepted (title, repo, workflow, variants, source) |
| `task_queued` | immediately after creation |
| `task_failed` | a queued task could not be started (e.g. its workflow became invalid) |
| `queue_paused`, `queue_resumed` | `queue pause/resume` or the TUI `p` key |
| `daemon_started`, `daemon_stopped` | engine daemon lifecycle (pid, version, Herdr socket) |
| `task_blocked_by_dependency`, `task_unblocked` | a dependency failed or was cancelled / the task may wait or start again |
| `dependencies_overridden` | `task unblock`: a human started a task regardless of its dependencies |
| `epic_created` | `epic create` (epic id, ADR path, ADR SHA-256, title) |
| `plan_proposed`, `plan_failed` | the planning task produced a valid plan (plan SHA-256) / failed |
| `plan_accepted` | a human accepted plan tasks (keys, created task ids, approved commands, plan SHA-256, note) |
| `plan_rejected` | plan or some of its tasks declined (keys, note) |
| `plan_regenerated` | `epic replan` (feedback, whether the ADR changed) |
| `plan_edited` | `epic edit` (new plan SHA-256) |
| `conformance_requested`, `conformance_reviewed` | `epic verify` queued / its result (open follow-up keys) |
| `epic_done` | every accepted task of the epic finished |
| `shift_started`, `shift_ended` | `shift start` (deadline, budget) / the shift ended at its deadline or token budget (reason, inbox size) |
| `learn_started` | `learn` queued a task (number of findings) |
| `issue_created` | `tracker sync` created a GitHub issue for a task (URL, epic) |
| `incident_task_created` | `incidents task` queued a fix task (provider, issue, URL) |

Run logs:

| Event | When |
| --- | --- |
| `run_started` | first start: workflow name and SHA-256, variant, repo, dry-run flag |
| `worktree_created` | worktree ready (path, branch, base SHA, `reused` after idempotent re-creation) |
| `git_exclude_updated` | `/.herdr-orchestrator/` added to `.git/info/exclude` |
| `herdr_workspace_opened` | the worktree was opened as a Herdr workspace |
| `dry_run` | what a dry-run step *would* have done |
| `step_started` | a step execution begins (exec id, attempt, type, runner) |
| `step_completed` | a step execution ends (status, attempt, duration, error) |
| `step_skipped` | e.g. a named check with nothing configured or detected |
| `step_failure_ignored` | failure under `continue_on_failure` |
| `policy_evaluated` | any policy decision (subject, decision, reason, rule ids) |
| `policy_violation` | a diff contained denied paths or escapes |
| `agent_started` | agent launched (mode, name, kind, pane, workspace) |
| `agent_attached` | an existing agent was reused or reattached |
| `agent_prompt_sent` | prompt submitted (SHA-256 of the prompt, pane) |
| `agent_blocked`, `agent_unblocked` | the agent waited for a human in its own UI |
| `agent_failed` | agent timed out, failed or was lost |
| `usage_recorded` | tokens/cost/runtime with provenance (pane agents: read from their session logs) |
| `budget_exceeded` | run usage passed `limits.max_tokens`/`max_cost_usd`; an approval follows |
| `review_completed` | review verdict and finding count, or the parse error |
| `acceptance_verified` | acceptance review: met / not met / unverifiable counts and each criterion |
| `plan_proposed`, `conformance_reviewed` | validated `output: plan` / `output: conformance` of a step |
| `plan_invalid`, `acceptance_invalid`, `conformance_invalid` | structured output failed validation (parse error) |
| `read_only_violation` | a plan/conformance step changed the worktree |
| `test_only_retry` | after a failed check the retry changed only tests (approval follows) |
| `agent_tool_checked` | Claude `PreToolUse` hook: a non-allow decision for a tool call (`blocked: true` for DENY) |
| `pr_feedback_detected` | PR watcher found new failing checks or review comments |
| `pr_followup_created` | `run followup`: follow-up task created (task, PR, failing checks, comments) |
| `agent_stuck`, `agent_progressing` | the watchdog handed a busy agent to a human (reason, pane) / the agent made progress again |
| `contract_written` | validated `output: contract` (files, check, criteria map) |
| `contract_probe` | the contract's check ran for the red proof (argv, exit code) |
| `contract_not_red` | the contract already passed on the base; sent back to its writer |
| `contract_locked` | contract hashed and locked (SHA-256, files, check, commit; `eval_case` for replays) |
| `contract_approved`, `contract_amended` | the first approval after the lock approved it / the approver edited contract files first (old and new hash) |
| `eval_case_recorded` | `eval record`: the run became an eval case (case id, contract SHA-256) |
| `approval_batch` | `approval batch` approved this run's ship-it step (approval id, risk and reasons) |
| `branch_update_merged` | `run update` merged the base into the branch (onto, SHA, conflicted files) |
| `files_changed` | changed paths with +/- counts at a policy gate |
| `command_requested` | a command/check is about to be evaluated (argv, shell, source) |
| `command_started` | process spawned (argv, pid, cwd, env **names** only) |
| `command_completed` | exit code, timeout/cancel flags, duration |
| `check_passed`, `check_failed` | outcome of check steps |
| `git_commit_created` | commit SHA and message |
| `git_push_started`, `git_pushed` | branch push |
| `pr_created` | PR URL (`already_existed` when found by idempotency check) |
| `approval_requested` | approval id, kind, reason, pending action, file count |
| `approval_granted`, `approval_denied` | human decision (decided by, note) |
| `approval_expired` | `limits.approval_timeout` elapsed (treated as denial) |
| `approval_reused` | an explicit approval covered this step's gated action |
| `retry_started` | `on_failure` routing, or a human `run retry` (`human: true`) |
| `retries_exhausted` | retry budget used up |
| `cancel_requested` | a human asked to cancel (reason) |
| `retry_requested` | a human asked to resume (from step) |
| `run_paused`, `run_resumed` | pause at a step boundary and resume |
| `run_completed` | success (head SHA, branch, PR, files changed, usage) |
| `run_failed` | failure reason |
| `run_blocked` | stopped for a policy violation |
| `run_needs_human` | stopped in an ambiguous state |
| `run_stopped` | other non-terminal stop |
| `run_cancelled` | cancellation completed |
| `worktree_removed` | worktree cleaned up after success (`git.cleanup_on_success`) or by `gc` (`via: gc`, reason) |
| `variant_selected` | a human selected this variant |
| `recovery_started`, `recovery_completed` | startup reconciliation (classification, reason, old/new status) |

Command output and agent transcripts are not in the audit log; they are in
`logs/<run_id>/<exec_id>.log` (redacted). The audit log references them
through the run document.

## Hash chain

```text
hash[n] = SHA-256( canonical_json(event[n] without "hash") ‖ previous_hash[n] )
previous_hash[0] = "0000…0000"   (64 zeros)
previous_hash[n] = hash[n-1]
```

`canonical_json` is the event serialized with lexicographically sorted
keys; `‖` is string concatenation of the lowercase hex hash. Changing,
reordering, inserting or deleting any event breaks verification of every
event after it.

Hash chaining is on by default and controlled by `audit.hash_chain` in the
global config (it applies to the whole state directory, so set it globally,
not per project). Disabling it is not recommended. Unchained files (`hash`
empty) are reported as "NOT chained" by `verify`.

## Verification

```bash
herdr-orchestrator audit verify '#12'          # a run (id, prefix or #task[variant])
herdr-orchestrator audit verify global
herdr-orchestrator audit verify --all          # every *.jsonl in the audit dir
herdr-orchestrator audit show '#12' --tail 20  # human-readable listing (--json for raw events)
```

```text
OK       …/audit/run-4a0a43ea0b01.jsonl (18 events, hash-chained)
TAMPERED …/audit/run-c2f440305764.jsonl (15 events)
         line 7: hash mismatch — event ev-… was modified
         line 8: previous_hash does not link to the prior event (event ev-…)
```

Checks per line: parseable, `seq` continuity from 0, `previous_hash` links
to the prior event, recomputed hash matches. Exit code `1` if any file
fails, `0` otherwise. `--json` prints one report per file.

### What this does and does not guarantee

- **Tamper-evident, not tamper-proof.** It detects edits, deletions and
  reordering *within* a file. Someone with write access can delete the whole
  file, truncate it at an event boundary (the remaining prefix still
  verifies), or rewrite and re-hash everything from some point on.
- **Not a signature.** Nothing proves *who* wrote an event; the chain has no
  secret. The `signature` field is reserved for a future signing key (e.g.
  an ssh-keygen or minisign key held outside the state directory). To anchor
  a log today, record the last `hash` somewhere else (a PR description, a
  ticket, a commit message) — `run pr` bodies include the verify command.
- Integrity of the *run documents* is protected separately by per-document
  SHA-256 envelopes (corrupt files are quarantined, see RECOVERY.md).

## Redaction

Before an event is written, its `data` is scrubbed:

- String values under keys that look secret (`password`, `passwd`,
  `secret`, `token`, `api_key`, `authorization`, `private_key`,
  `credential`, `cookie`) are replaced with `[REDACTED]`. Numbers and
  booleans are kept (so `cached_tokens: 42` stays readable).
- All strings are pattern-scrubbed for PEM private keys, `Authorization`
  headers and bearer tokens, Anthropic (`sk-ant-…`) and OpenAI (`sk-…`)
  keys, GitHub tokens (`gh[pousr]_…`, `github_pat_…`), AWS key ids and
  secret keys, Slack tokens, `PASSWORD=…`/`TOKEN: …`-style assignments and
  credentials embedded in URLs.
- Environment dumps are never logged: `command_started` records variable
  **names** only.

Command logs and agent transcripts pass through the same pattern redaction
(line by line). Redaction is best-effort: it lowers the chance of persisting
a credential; it cannot prove its absence.
