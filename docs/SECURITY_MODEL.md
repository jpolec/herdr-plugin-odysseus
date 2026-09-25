# Security model

herdr-orchestrator runs coding agents and commands **as your user, on your
machine**. It is a governance layer: it decides, records and gates what *it*
does, and it inspects what agents *did*. **It is not a sandbox.** Read this
before trusting it with anything that matters.

See also: [THREAT_MODEL.md](THREAT_MODEL.md), [POLICY_ENGINE.md](POLICY_ENGINE.md),
[AUDIT.md](AUDIT.md).

## 1. Two trust situations, never conflated

### A. Orchestrator-controlled actions — pre-flighted

Everything the orchestrator itself executes passes through
`normalize → policy → ALLOW | REQUIRE_APPROVAL | DENY` **before** it happens,
and every outcome is audited (`policy_evaluated`):

| Action | Where | Policy subject |
| --- | --- | --- |
| `command` / `check` steps (incl. named checks `tests`/`lint`/`security`) | `engine/steps.rs::command_step` | `action: command`, normalized argv + semantic tags (`policies/command.rs`) |
| Agent launch | `engine/steps.rs::agent_step` | `action: agent_start`, agent kind + launch args (e.g. denies `--dangerously-skip-permissions`) |
| Worktree creation | `engine/driver.rs::prepare` | `action: worktree_create` |
| Git commit | `engine/steps.rs::commit_changes` | `action: git_commit` |
| Git push | `engine/steps.rs::push_branch`, `engine/handoff.rs` | `action: git_push` (default: approval) |
| Pull request | `engine/steps.rs::pr_step`, `engine/handoff.rs` | `action: github_pr` (default: approval) |

DENY fails the step safely and stops the run as `blocked` **without** retry
routing (a denied command would only be denied again). REQUIRE_APPROVAL
pauses the run with a durable approval request.

### B. Actions inside agent processes — *not* intercepted

Claude, Codex, OpenCode etc. run their own tools (shell, file edits, network)
inside their own process in a Herdr pane. Herdr offers no hook to intercept
those actions, and neither do we. What we actually do:

1. **Launch with conservative permission modes** (`runners/profiles.rs`):
   `claude --permission-mode acceptEdits` (edits proceed, shell commands ask
   the human), `codex --sandbox workspace-write --ask-for-approval on-request`.
   When the agent asks, Herdr reports it as `blocked`; the step becomes
   `awaiting_human` and you are notified. **We never answer an agent's
   prompt on your behalf.**
2. **Confine cwd** to the run's own git worktree.
3. **Diff-based policy gate** after every agent step (and in explicit
   `type: policy` steps): every changed, added, deleted or untracked path in
   the worktree (relative to the base SHA) is evaluated as `write`/`delete`,
   plus an aggregate `diff_summary` (large deletes). Paths are checked for
   containment with `security::paths::check_containment`, which resolves
   symlinks component by component; a symlink escaping the worktree is itself
   a violation. DENY → run `blocked`, changes are **not committed**.
   REQUIRE_APPROVAL → human approval before commit.
4. **Audit** every step, agent start/prompt/block/unblock, file change and
   decision.
5. **Stop**: cancel interrupts the agent (`esc`/`ctrl+c` via Herdr) and
   terminates orchestrator-run processes (SIGTERM, then SIGKILL, whole
   process group).

This catches *files* an agent wrote, after the fact. It does **not** prevent
an agent from running `curl`, reading `~/.ssh`, or pushing with its own git
credentials during its turn if its own permission mode allows it — except
for Claude, see below.

On top of the gate, the diff is checked for changes that weaken the checks
themselves: agent instruction and orchestrator policy files, deleted tests,
test runner configuration, added lines that skip or ignore tests
(`added_lines` rules), a retry that changed only tests after a failed check
(`guard-test-only-retry`), and paths outside a task's declared scope
(`task-scope`). All of these ask a human; see POLICY_ENGINE.md.

#### Claude: real-time DENY through its `PreToolUse` hook

With `guard.claude_hook: true` (default) the orchestrator starts Claude
agents (pane and headless) with
`--settings <worktree>/.herdr-orchestrator/claude-settings.json`. That file
(git-excluded, never in the project's `.claude/`; Claude adds `--settings`
hooks to the user's own) registers
`herdr-orchestrator hook claude-pretool --run <id>` for the tools `Bash`,
`Write`, `Edit`, `MultiEdit`, `NotebookEdit` and `Read`. Claude runs it
before each such call and sends the call on stdin; the hook evaluates it
against the run's effective policy:

- **DENY** → `permissionDecision: "deny"`: the call does not run, the reason
  goes to the agent, and `agent_tool_checked` is audited. This applies in
  every permission mode, including `acceptEdits`.
- **REQUIRE_APPROVAL / ALLOW** → no decision. Claude's own permission mode
  decides, and the diff gate still runs, so nothing is asked twice.

What it does **not** cover: other agents (Codex, OpenCode… — still only
their own sandbox plus the diff gate), Claude tools outside the list above
(web fetch, MCP tools, subagents' own tool calls follow Claude's hook
semantics), and anything once a command is running (`Bash` is judged by its
text and tags, not by what the program then does). The hook runs the
orchestrator binary with the run's state and config directories. It
**fails open**: if it cannot load the run or the policy it prints an error
and makes no decision — the diff gate remains the backstop. Turn it off
with `guard.claude_hook: false`.

#### Contract lock (contract-first runs)

Once a contract is locked (see WORKFLOWS.md, `output: contract`), its files
are protected in two layers: Claude's file tools are refused in real time by
the hook (`contract-lock`), and for every agent the diff gate compares the
files' content with the locked SHA-256 — any difference is a DENY, the run
is `blocked` and nothing is committed or pushed. A shell command that edits
a contract file (any agent, including Claude's `Bash`) is **not** stopped
while it runs; it is caught by the hash check at the next diff gate, and the
PR step re-checks the hashes before pushing. The receipt in the PR body and
`receipt verify` rest on the local hash-chained audit log: tamper-evident on
this machine, **not** a signature another machine can verify.

#### Watchdog

The watchdog only observes (pane text, worktree fingerprint, session-log
token counts) and, when it fires, moves the step to `awaiting_human`. It
never types into the agent, interrupts it or fails the step.

### What we can observe vs. enforce

| Capability | Orchestrator commands | Agent-internal actions |
| --- | --- | --- |
| Block a command before it runs | ✅ policy pre-flight | ⚠️ Claude: DENY rules via its `PreToolUse` hook; others: agent's own permission mode only |
| Require human approval before a command | ✅ | ⚠️ agent's own prompt (surfaced as `blocked`) |
| Detect file writes/deletes | ✅ diff gate | ✅ diff gate, at step boundaries only |
| Prevent a secret file from being committed | ✅ | ✅ (diff DENY → never committed) |
| Prevent a secret file from being *written* | ✅ (not by us) | ⚠️ Claude: blocked by the hook; others: detected afterwards |
| Detect symlink escapes | ✅ | ✅ at step boundaries |
| Observe network access | ⚠️ command tags (`curl`, `ssh`…) | ❌ |
| Observe reads of credential stores | ⚠️ command tags / `read` path rules | ⚠️ Claude `Read`/`Bash` via the hook; others ❌ |
| Control environment variables | ✅ allowlist | ❌ pane agents inherit Herdr's pane env |
| Stop/interrupt | ✅ process group kill | ✅ interrupt keys via Herdr / close pane |
| Audit | ✅ | ✅ lifecycle + resulting diff, not individual tool calls |

### Project memory is untrusted context

The "Prior work" section in agent prompts is built from recorded facts
(changed files, failure reasons, approved contracts), human text (denial
notes, `note add`) and agent text (review findings, `notes_for_others`).
Agent-written notes therefore reach other agents: a prompt-injection path
between agents. Mitigations: the section is explicitly labelled as history,
not instructions; each record is one short line (notes ≤ 600 characters,
at most five per result, secrets redacted); records need file or word
overlap with the task to appear; every injection is audited with the record
ids; `note list` / `note rm` let a human inspect and remove notes;
`memory.history: false` turns it off. Raw conversations are never read.

## 2. Sandboxing

`security/sandbox.rs` defines `ExecutionSandbox` with a `wrap(argv)` seam.
Only `none` is implemented; configuring `bubblewrap`, `container` or
`macos_profile` is an error. With `none`, commands run as your user with the
worktree as cwd and a filtered environment. That is **not isolation**.

## 3. Environment

Orchestrator-run commands and headless agents get an explicit environment
(`security/env.rs`), never the full parent environment:

- default allowlist: `PATH HOME USER LOGNAME SHELL LANG LC_* TERM COLORTERM
  TMPDIR TZ XDG_* SSH_AUTH_SOCK CARGO_HOME RUSTUP_HOME GOPATH NVM_DIR HERDR_*`
- plus `environment.set`, plus `HERDR_ORCH_*` run variables
- per-runner `env_inherit` (e.g. `ANTHROPIC_*` for claude headless)
- `inherit_all: true` exists as an explicit escape hatch
- only variable *names* are ever logged (`command_started.env_keys`)

**Pane-mode agents are different:** they are launched by Herdr inside a
Herdr pane shell and inherit **Herdr's pane environment**, not our allowlist.
We add `HERDR_ORCH_*` variables to the pane; we cannot remove what Herdr
provides.

## 4. Git and GitHub

- One worktree + branch per run; base SHA captured before creation; branch
  names validated (`git check-ref-format` plus stricter rules); existing
  branches and non-empty paths are never reused or reset.
- Orchestrator commits run with `-c core.hooksPath=/dev/null` and
  `--no-verify`, so a repository cannot gain code execution through our
  commits or pushes. (Agents running `git` themselves are not covered.)
- Never force-push: pushes use an explicit `refs/heads/B:refs/heads/B`
  refspec, and arguments starting with `-`/`+` or containing `:` are refused.
  The default policy additionally denies force-push forms in commands.
- Worktrees with uncommitted changes are never removed.
- PRs are **draft** by default. `github.auto_merge: true` is rejected by
  config validation. Nothing merges or deploys.

## 5. Templates and command construction

- Commands are argv arrays executed directly (`process.rs`), never via
  `sh -c`, unless the step says `shell: true` — which the default policy
  sends to human approval and which is evaluated per `&&`/`||`/`;`/`|`
  segment and `$(…)` substitution.
- Templating is `{{name}}` substitution only (`workflow/template.rs`):
  no expressions or filters; values are never re-expanded.
- Untrusted variables — `task`, `task_title`, `previous.output`, `feedback`,
  `step.<id>.output` — are **rejected at validation** in command argv and
  command env. Templates are rejected in `argv[0]` and entirely in shell
  scripts (use the `HERDR_ORCH_*` env vars instead).
- Runner placeholders (`{{prompt}}`, `{{prompt_file}}`, `{{output_file}}`)
  substitute **whole argv elements** only; a prompt starting with `-` is
  prefixed with a space so it cannot become an option.
- Agent output files are read only if they are regular files ≤ 1 MiB
  (symlinks refused).

## 6. Approvals

- Durable, one-shot, with full context (files, diff stat, checks, policy
  reasons, exact pending action). A second decision on the same request is
  refused.
- Approving never changes policy. There is no "always allow" in this
  version.
- An explicit `approval` step covers the REQUIRE_APPROVAL decisions of the
  **immediately following step only**, and only while the worktree HEAD is
  unchanged (`approval_cover`, audited as `approval_reused`). DENY is never
  covered.
- Approvals from the CLI/TUI record the human (`$USER`) as actor. `run pr`
  invoked by a human is itself the approval and is audited as such.
- Optional `limits.approval_timeout`: expiry **denies**, never approves.
- `approval batch --max-risk …` (and `A` in the pane) approves only
  workflow `approval` steps ("ship it?") whose run is at or below that risk
  level; it never approves a policy question (REQUIRE_APPROVAL from a rule),
  and each batch approval is audited with the risk and its reasons.

## 7. Local data protection

- State dir mode `0700`; state, audit and log files `0600`; writes are
  atomic with fsync; documents carry a sha256 and are quarantined if corrupt.
- Daemon control socket `0600`. If the state path is too long for a Unix
  socket, the socket lives in `/tmp/herdr-orch-<uid>/` (created `0700`,
  owner-checked).
- Audit events and command logs pass through `audit/redact.rs`: patterns for
  Authorization/Bearer headers, AWS/GitHub/OpenAI/Anthropic/Slack tokens,
  `*PASSWORD*=`/`*TOKEN*=` style assignments, URL credentials, PEM private
  keys; string values under sensitive key names are replaced. Numbers and
  booleans are never redacted. Redaction is **best-effort**.
- Audit logs are hash-chained (tamper-evident, not signed). See AUDIT.md.

## 8. Network and telemetry

No telemetry, analytics, crash upload or update checks. The binary itself
makes no network connections; network access happens only through tools you
configured it to run (`gh`, `git push`, agent CLIs, your commands). `doctor`
stays offline unless `--network` is given. The optional PR watcher
(`github.watch_prs`) calls `gh` for open PRs of recent runs; it only
notifies.

Token usage of pane agents is read from the agents' own local session logs
(`$CLAUDE_CONFIG_DIR` or `~/.claude/projects`, `$CODEX_HOME` or
`~/.codex/sessions`): read-only, bounded (files over 512 MiB are skipped),
only usage counters and model names are kept, nothing leaves the machine.
Session ids are never used as paths unless they are plain identifiers.
Off with `usage.session_logs: false`.

Integrations, all opt-in and started by you (or by `auto_sync`):

- **GitHub tracker** (`tracker sync/import`) uses `gh`. Issue text imported
  as tasks is untrusted and only reaches prompts. Issues and comments the
  orchestrator writes are redacted. Adding items to a Projects board needs
  the `project` token scope, which the orchestrator never requests itself.
- **Sentry** (`incidents`) calls the Sentry API with `curl`. The token is
  read only from `SENTRY_AUTH_TOKEN` and passed to `curl` as a config on
  stdin, never in its arguments (not visible in `ps`); `sentry.url` must be
  https. Data is minimised: the issue's title, culprit, counts, first/last
  seen, `release`/`environment`/`runtime`/`os.name` tags and each
  exception's type, message (≤ 500 chars, redacted) and in-app stack frames.
  Requests, users, breadcrumbs, contexts and other tags are never read into
  a prompt.
