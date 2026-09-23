# Roadmap

Status: proposals, not implemented. Nothing here is promised by 0.1.x. Each
item says what it builds on in the current code so the cost is visible.
Ordering is by value for day-to-day use; §1 is the largest and most requested.

## 1. ADR → plan → accepted tasks → verified outcome ("epics")

**Goal.** Point the orchestrator at an ADR (or a directory of them). An agent
reads it and proposes a plan: small tasks with acceptance criteria,
verification and dependencies. You accept, edit or reject the plan. The
accepted tasks go through the usual workflow. Each task is checked against
its own acceptance criteria, and the whole epic is checked against the ADR.

`plan` is already the dry-run command, so this feature is called **epic**.

### 1.1 Flow

```text
ADR file(s) ──► planner agent (read-only) ──► proposed plan (validated JSON)
                                                   │
                           human: accept all / some / edit / regenerate with feedback
                                                   ▼
             accepted tasks, queued with depends_on (DAG) ──► normal workflow per task
                                                   │        implement → tests → acceptance review (gated)
                                                   ▼
                     epic conformance review: combined result vs. the ADR's Decision
                                                   │
                    gaps → proposed follow-up tasks (accepted by you again, never auto-started)
```

### 1.2 Finding ADRs

- `adr.dirs` in config. Default search: `docs/adr`, `doc/adr`,
  `docs/decisions`, `adr`, `.ai/adr`.
- Parse the common formats (Nygard, MADR): title, `Status`, `Context`,
  `Decision`, `Consequences`. Unknown layouts still work as plain text.
- `herdr-orchestrator adr list` shows each ADR with its status, and whether
  it has an epic and how far that epic has got. `Superseded`, `Rejected` and
  `Deprecated` ADRs are hidden unless you pass `--all`.

### 1.3 Planning

```bash
herdr-orchestrator epic create --from docs/adr/0007-rate-limiting.md [--runner claude]
herdr-orchestrator epic create --from docs/adr/ --status proposed,accepted   # one epic per ADR
```

The planner is an ordinary agent step with a new `output: plan`, run in a
throwaway worktree at the base SHA. Its prompt tells it not to change files.
The diff policy gate enforces that: any change fails the planning step. It
returns:

```json
{
  "adr": {"path": "docs/adr/0007-rate-limiting.md", "decision_summary": "…"},
  "tasks": [
    {
      "key": "T1",
      "title": "Token-bucket limiter in webhooks middleware",
      "description": "…",
      "acceptance": [
        "Requests over 100/min per API key get HTTP 429 with Retry-After",
        "Limit is configurable via RATE_LIMIT_PER_MIN"
      ],
      "verification": {
        "checks": ["tests", "lint"],
        "commands": [["cargo", "test", "rate_limit"]],
        "manual": ["Load test staging with k6 script in scripts/k6"]
      },
      "depends_on": [],
      "adr_refs": ["Decision §2"],
      "workflow": "implement-review",
      "risk": "low"
    }
  ],
  "out_of_scope": ["Distributed limiter (ADR says: later)"],
  "open_questions": ["Should admin keys be exempt?"]
}
```

Validation, in the same way as `output: review`: unique keys, `depends_on`
refers to existing keys and has no cycles, at most `epic.max_tasks` tasks
(default 12), each task has at least one acceptance criterion, and known
workflow names and check names. If the plan fails validation, the agent is
sent the errors and retries (bounded). A plan is never partly accepted just
because it was partly valid.

### 1.4 Human acceptance

The plan is a durable document, `state/epics/<id>.json`, with an optional
committed copy at `.ai/herdr-orchestrator/epics/<adr-slug>.yaml` so it can be
reviewed in git.

- TUI: a new *Epics* screen with the task list, dependency order, and the
  acceptance criteria and verification for each task. Keys: `space` toggles a
  task, `e` edits the plan in `$EDITOR` (YAML, re-validated on save), `g`
  regenerates with your comment as feedback, `y` accepts the selection, `n`
  rejects.
- CLI: `epic show <id>`, `epic accept <id> [--only T1,T3]`, `epic edit <id>`,
  `epic reject <id> --note "…"`.
- **Commands proposed by the planner are untrusted.** They come from ADR
  text and agent output (THREAT_MODEL §§1–2). They go through the normal
  policy pre-flight. The acceptance screen lists every command separately,
  and accepting the plan is how you approve them. The existing template trust
  rules still apply: plan text never goes into argv.
- Open questions from the plan are shown first. You can answer them in the
  editor, and the answers become part of every task's prompt.

### 1.5 Execution with dependencies

- `Task` gains `epic_id` and `depends_on: [task_id]`. The scheduler already
  picks the next task FIFO under `max_parallel_runs`. It will skip tasks
  whose dependencies are not satisfied yet. A dependency that failed or was
  cancelled blocks its dependents (`blocked: dependency #12 failed`); nothing
  cascades silently.
- What "satisfied" means is set by `epic.dependency_mode`:
  - `merged` (default): the dependency's PR is merged into the base.
    `gh pr view --json state` is polled in the daemon's existing tick. This
    is the safest option, and every task branches from an up-to-date base.
  - `stacked`: the dependent task branches from the dependency's branch and
    its draft PR targets that branch. This is faster, but you have to review
    in order.
- Tasks that do not depend on each other run in parallel, one worktree each,
  as today.

### 1.6 Verification

- A new template variable, `{{acceptance}}` (untrusted, prompt-only), gives
  the implementer the numbered criteria.
- The task's `verification.checks` and `verification.commands` become
  `check` steps in the task's run snapshot, inserted before the review.
- A new structured output, `output: acceptance`, is used by the reviewer
  step (built-in skill `acceptance-review`):

  ```json
  {"criteria": [
     {"index": 1, "status": "met" | "not_met" | "unverifiable",
      "evidence": "tests/rate_limit.rs::over_limit_returns_429"}
   ],
   "verdict": "approved" | "changes_requested"}
  ```

  With `gate: true`, a criterion marked `not_met` goes back to the
  implementer as feedback, using the same retry routing that review findings
  use today. An `unverifiable` criterion is not a failure. It appears in the
  approval screen, so a human checks it.
- The approval request gains a criteria table (met / not met /
  unverifiable, with evidence) next to the diff stat and policy reasons.
  `manual` verification items are listed there as a checklist, and each item
  must be ticked before `y` is accepted.

### 1.7 Epic-level conformance and drift

- When every accepted task has finished, a conformance review compares the
  combined diff (base..last merged or stacked head) with the ADR's
  *Decision* and *Consequences*. Each point is marked covered / partially
  covered / missing. Missing points become **proposed** follow-up tasks,
  which go back to §1.4 for acceptance.
- The ADR's content hash is stored when the plan is accepted. If the ADR
  changes later, `adr list` and the Epics screen show *ADR changed since the
  plan*, and `epic replan <id>` proposes changes only for the differences
  (added, changed or dropped tasks). No running task is changed.
- Optionally (`epic.propose_adr_status: true`), the orchestrator proposes a
  final task that updates the ADR's `Status` to *Implemented* and links the
  PRs. It needs approval like any other task and is never written directly.

### 1.8 Audit and state

New audit events: `epic_created`, `plan_proposed` (plan hash),
`plan_accepted` (task keys, approved commands, human), `plan_rejected`,
`task_blocked_by_dependency`, `acceptance_verified` (per-criterion result),
`conformance_reviewed`, `adr_drift_detected`. The epic document follows the
store's existing envelope and migration rules.

### 1.9 Suggested phases

1. `adr list`, `epic create` with `output: plan` validation, and CLI
   accept/reject. Accepted tasks are created as independent tasks (no DAG
   yet).
2. `depends_on` in the scheduler with `merged` mode; the TUI Epics screen.
3. `{{acceptance}}`, `output: acceptance`, the criteria table in approvals,
   and the manual checklist.
4. Conformance review, drift detection, and follow-up proposals;
   `stacked` mode.

Testing follows the current approach: a `fake:plan` runner that writes a
fixed plan (valid, cyclic, or too large), a `fake:acceptance-*` runner, and
the fake `gh` extended with `pr view --json state` for the `merged` mode.

## 2. What is still worth taking from Cezar

Cezar ([open-mercato/cezar](https://github.com/open-mercato/cezar)) was the
functional reference. Already covered: queue, worktree per task, YAML
workflows with retry, skills (`.ai/skills/`), variants and comparison, and
draft PRs. Still missing here:

| Cezar feature | Proposal here | Notes |
| --- | --- | --- |
| One-click "give this GitHub issue to an agent" | TUI **Issues inbox**: `gh issue list` (optionally filtered by a label such as `agent-ready`), `enter` opens the new-task form pre-filled | The CLI already has `task create --from-issue`; the TUI does not. It never starts tasks by itself. |
| Attach files to a task | `task create --attach path…` copies files into `<worktree>/.herdr-orchestrator/in/` (git-excluded, size-capped) and lists them in the prompt | Screenshots and logs for bug reports |
| Live tokens and cost | For pane agents, read the agent's own session log (Claude `~/.claude/projects/…/*.jsonl`, Codex sessions) and record usage as `reported` | Today pane agents report `unknown` (ARCHITECTURE §12). This would make `limits` budgets meaningful. |
| Autonomous mode ("never stops to ask") | **Not as-is.** Instead: persistent, scoped, expiring approval rules, e.g. "push to `herdr/*` and open draft PRs in this repo for 7 days", created from the approval screen and audited | Already listed as an MVP cut. DENY rules can never be weakened this way. |
| `init` scaffolding | Extend `config init` with `--with-workflow` and `--with-skill` to write commented starter files | Small |
| Workflow builder UI | Low priority. `workflow validate` plus `plan` already give quick feedback. | — |
| Web or mobile cockpit, VPS install | Out of scope. The plugin is local and Herdr-native by design. | — |

## 3. Other additions

Ordered by value to effort.

1. **Real-time policy for Claude through its hooks.** Claude Code supports
   `PreToolUse` hooks. When the pane runner starts Claude, it can write a
   git-excluded `.claude/settings.local.json` into the worktree whose hook
   calls `herdr-orchestrator policy check --command … / --path …`. A DENY
   would then stop the tool call *before* it runs, not only at the next diff
   gate. This addresses most of UPSTREAM_REQUESTS §4 for one agent without
   waiting for Herdr. It would be opt-in and clearly labelled, and the
   SECURITY_MODEL would say exactly what it covers.
2. **Follow-up on PR review and CI.** After a draft PR is opened, the daemon
   watches `gh pr checks` and review comments. Failing CI or new comments
   become a proposed "address feedback" run on the same branch, with the
   comments as `{{feedback}}`. You confirm it with one key. This closes the
   loop after the handoff, which is where most of the manual work is today.
3. **Worktree garbage collection.** `herdr-orchestrator gc` lists finished
   runs whose branch is merged or whose PR is closed, and removes their
   worktrees after confirmation. It never removes a dirty worktree (same
   rule as today).
4. **`parallel` step type** (reserved today): for example, lint, tests and
   the security scan at the same time, or two reviewers (Claude and Codex)
   whose findings are merged.
5. **Visible command panes.** Run `check` steps in a Herdr pane so test
   output can be watched live (already on the ARCHITECTURE §19 list).
6. **Learning from reviews.** Every N runs, summarize recurring review
   findings (same rule, same directory) into a *proposed* change to a
   project skill (`.ai/skills/*.md`), shown as a diff that you approve. It is
   never applied automatically.
7. **Runner statistics.** Show locally, per runner and workflow: success
   rate, average retries, and review verdicts. This helps choose runners and
   the variant mix, using only local data.
8. **Scheduled tasks.** For example, "update dependencies every Monday"
   through `[[startup]]` and the daemon tick. Each run still stops for
   approval as usual.
9. **Sandbox backends** behind the existing `ExecutionSandbox` seam:
   `sandbox-exec` profiles on macOS, bubblewrap on Linux.
10. **Signed audit** using the `signature` field the event schema already
    reserves.
