# Threat model

Scope: herdr-orchestrator v0.1 running locally as a Herdr plugin, driving
agent CLIs in Herdr panes and commands in git worktrees. Assets: the user's
source code and git history, credentials on the machine, remote repositories,
production systems reachable from the machine, and the integrity of the
orchestrator's own state and audit trail.

Trust assumptions:

- The **user** and their machine are trusted.
- **Herdr** and the **agent CLIs** are trusted to the extent the user already
  trusts them; we cannot contain them.
- **Global config** (`$HERDR_PLUGIN_CONFIG_DIR`) is trusted.
- **Project config, workflows, skills and policies inside a repository**
  are trusted input *from the repository owner*. Running tasks on a
  repository you don't trust is equivalent to running its build scripts.
- **Task text, GitHub issue text, repository file contents and agent output**
  are untrusted data.

Likelihood: L = low, M = medium, H = high (for a typical single-developer
setup).

---

### 1. Malicious repository instructions
- **Threat:** files like `AGENTS.md`, `CLAUDE.md`, READMEs instruct agents to
  do harmful things (exfiltrate, disable tests, push).
- **Impact:** high — agents act with the user's privileges.
- **Likelihood:** M for third-party repos, L for own repos.
- **Mitigation:** agents launched with ask-before-shell permission modes
  (`runners/profiles.rs`); worktree-scoped cwd; diff policy gate after each
  agent step (`engine/steps.rs::diff_policy_gate`); independent review step
  in shipped workflows; orchestrator never pushes/opens PRs without policy
  (approval by default). Changes to agent instruction files and orchestrator
  policy, deleted tests, test runner config and added test skips need
  approval (default policy); Claude's `PreToolUse` hook blocks DENY-level
  tool calls before they run.
- **Residual:** an agent that is allowed to run shell commands (by the user
  answering its prompts, or a looser permission config) can do anything the
  user can, including network access. Not preventable here.

### 2. Prompt injection from repository files
- **Threat:** content read during the task (code, docs, test fixtures, issue
  bodies) hijacks the agent.
- **Impact:** high. **Likelihood:** M.
- **Mitigation:** same as #1; the orchestrator never *interprets* agent or
  repo text as instructions — outputs only fill `{{...output}}` template
  slots in *prompts*, and untrusted variables are banned from command lines
  (`workflow/mod.rs::validate`, `workflow/template.rs`). Review output is
  schema-validated; invalid output is flagged, gated reviews fail closed.
- **Residual:** prompt injection against the model is not solvable by the
  orchestrator. Human approval before PR is the backstop.
- **ADRs, epic plans and PR feedback** are the same kind of input. ADR text,
  plan tasks and GitHub review comments only reach prompts (`{{task}}`,
  `{{acceptance}}`); planning and conformance agents are read-only (any
  change fails the step). Planner-proposed verification commands are shown
  at `epic show` and become check steps only when a human accepts the plan,
  must be template-free argv arrays, and are still policy-checked when they
  run. Nothing from a plan or a PR starts without a human.

### 3. Malicious dependency install scripts
- **Threat:** `npm install`, `pip install`, build scripts run arbitrary code.
- **Impact:** high. **Likelihood:** M.
- **Mitigation:** orchestrator commands are tagged `package_install`
  (`policies/command.rs`) so users can require approval; default env
  allowlist withholds API keys and cloud credentials from commands
  (`security/env.rs`); lockfile changes above 500 lines need approval.
- **Residual:** default policy *allows* package installs, and agents may run
  installs themselves. No sandbox (`security/sandbox.rs` only `none`).

### 4. Malicious plugin configuration
- **Threat:** a crafted `config.yaml` / policy loosens safety (e.g. a
  project config trying to drop global deny rules or enable auto-merge).
- **Impact:** high. **Likelihood:** L.
- **Mitigation:** `deny_unknown_fields` everywhere; `github.auto_merge: true`
  rejected (`config::validate`); policy files concatenate across layers and
  `default_decision` can only become stricter (`PolicySet::add`);
  `policy.replace_global` is an explicit, visible key; rules that match
  everything are rejected; `config show --layers` shows provenance.
- **Residual:** project config is trusted input; `replace_global: true` or
  `builtin_default: false` in a repo you don't control *can* weaken policy.
  Review `.ai/herdr-orchestrator/` in untrusted repos.

### 5. Command injection
- **Threat:** task/issue text or agent output ends up in a command line.
- **Impact:** high. **Likelihood:** L (by construction).
- **Mitigation:** argv execution without shell (`process.rs`); untrusted
  template variables rejected in argv/env at workflow validation; no
  templates in `argv[0]`; runner placeholders replace whole elements only,
  dash-leading prompts neutralised (`runners/headless.rs::build_argv`);
  `gh`/`git` wrappers refuse dash-leading refs (`github/mod.rs`,
  `git/mod.rs`); IDs used in paths are validated (`store::valid_id`).
- **Residual:** trusted variables (paths, branch names) are still
  interpolated; they are generated by us and validated.

### 6. Shell interpolation
- **Threat:** `shell: true` steps or `sh -c` wrappers hide dangerous
  commands.
- **Impact:** high. **Likelihood:** L.
- **Mitigation:** templates forbidden in shell scripts; default policy
  requires approval for any `shell: true` step; scripts are split into
  `&&`/`||`/`;`/`|`/newline segments and `$(…)`/backtick substitutions, each
  evaluated; `sh -c`/`bash -lc` argv is unwrapped (`policies/command.rs`).
- **Residual:** the splitter is not a full shell parser (e.g. `eval`,
  variables holding commands, heredocs). Most-restrictive-wins plus approval
  is the backstop.

### 7. Symlink attacks
- **Threat:** an agent creates `link -> ~/.ssh` or `link -> /etc` so that a
  later step reads/writes outside the worktree, or to disguise a write.
- **Impact:** M–H. **Likelihood:** L.
- **Mitigation:** `security::paths::check_containment` walks every existing
  path component, resolving symlinks (including dangling ones), and reports
  escapes; the diff gate treats escapes as DENY; agent output files are read
  only if they are regular files (symlinks refused, `runners/mod.rs`);
  `cwd:` of command steps must resolve inside the worktree.
- **Residual:** detection is at step boundaries; an agent could follow a
  symlink during its own turn.

### 8. Worktree traversal
- **Threat:** crafted names/paths escape the worktree root (`../`), or
  string-prefix tricks (`/repo/wt-evil`).
- **Impact:** M. **Likelihood:** L.
- **Mitigation:** slugified names (`git::slugify`), branch validation,
  `git.worktree_root` may not contain `..`, `plan_names` asserts
  containment; containment uses component comparison, not string prefixes;
  `cwd:` rejects absolute/`..` paths at validation.
- **Residual:** an absolute `git.worktree_root` in config is allowed (trusted
  config).

### 9. Secret exfiltration
- **Threat:** secrets leak via agent network calls, logs, audit, PR bodies,
  or committed files.
- **Impact:** high. **Likelihood:** M.
- **Mitigation:** DENY on writing/deleting secret-looking files (`.env*`,
  keys, `secrets/`…) and on commands referencing credential stores;
  denied changes are never committed; env allowlist for orchestrator
  commands; redaction of audit events, command logs, transcripts and PR
  bodies (`audit/redact.rs`); env values never logged (names only).
- **Residual:** pane agents inherit Herdr's full pane environment and can
  read any file the user can and use the network; redaction is
  pattern-based and best-effort; a secret written into an ordinary source
  file is not recognised by path rules.

### 10. Compromised agent CLI
- **Threat:** a malicious or buggy `claude`/`codex` binary.
- **Impact:** critical. **Likelihood:** L.
- **Mitigation:** none beyond detection: diff gate, audit, human review
  before push/PR, `doctor` shows resolved versions.
- **Residual:** a compromised agent binary owns the user account. Out of
  scope; use OS-level controls.

### 11. Rogue workflow
- **Threat:** a workflow that runs destructive commands, pushes, or loops.
- **Impact:** high. **Likelihood:** L–M.
- **Mitigation:** strict schema validation (`deny_unknown_fields`, typed
  steps); every command pre-flighted by policy (force push, `reset --hard`,
  `rm -rf /`, `terraform destroy`, prod targets denied); retries bounded by
  `on_failure.max_attempts ≤ 10` and `limits.max_retries`; `max_runtime`;
  retry targets must be at or before the failing step and cannot be
  approval/PR steps; runs execute the workflow **snapshot** stored with its
  sha256 in the run, so later edits don't change a running run.
- **Residual:** a workflow can still run any command the policy allows.

### 12. Malicious skill file
- **Threat:** a skill (`.ai/skills/*.md`) with harmful instructions.
- **Impact:** high. **Likelihood:** L–M.
- **Mitigation:** skill names validated (no traversal), size cap 256 KiB,
  source recorded (`workflow/catalog.rs`); skills only become prompt text —
  same containment as #1/#2.
- **Residual:** skills are trusted prompt content; review them in untrusted
  repos.

### 13. Git hooks
- **Threat:** repository `.git/hooks` or `core.hooksPath` execute code when
  the orchestrator commits/pushes.
- **Impact:** high. **Likelihood:** M.
- **Mitigation:** orchestrator commits and pushes use
  `-c core.hooksPath=/dev/null` and `--no-verify` (`git/mod.rs`), covered by
  a test that a failing pre-commit hook does not run.
- **Residual:** other git invocations by the orchestrator (status, diff,
  worktree add) may still trigger hooks configured to run on them
  (e.g. `post-checkout` on `worktree add`); agents running git themselves
  are not covered.

### 14. Arbitrary process execution
- **Threat:** the orchestrator is used to run arbitrary processes.
- **Impact:** high. **Likelihood:** L.
- **Mitigation:** it only executes argv from workflows/config/runner
  profiles, never from task text or agent output; all through policy;
  process groups are killed on timeout/cancel; recovery never re-runs an
  interrupted `command` step automatically (`recovery/mod.rs`).
- **Residual:** anyone who can edit your config or workflows can run
  commands as you — same as any build tool.

### 15. Remote push
- **Threat:** unwanted pushes, force pushes, pushing secrets.
- **Impact:** high. **Likelihood:** M.
- **Mitigation:** default policy requires approval for `git_push` and
  `github_pr`; force/delete push forms denied by tag
  (`git_force_push`); push helper uses an explicit non-`+` refspec and no
  `--force`; diff gate runs before commit; PR idempotency check avoids
  duplicates after restarts.
- **Residual:** agents with git credentials can push themselves if their
  permission mode allows it.

### 16. Production command execution
- **Threat:** deploys, `kubectl`/`terraform` against production.
- **Impact:** critical. **Likelihood:** L–M.
- **Mitigation:** tags `production_target` (infra tools with prod-looking
  args), `terraform_destroy`, `kubectl_delete_namespace` → DENY;
  `terraform_apply`, `kubectl_mutation`, `deploy` → approval; nothing
  auto-deploys.
- **Residual:** "production" detection is heuristic (`prod`,
  `production`, `prd` words); environments named otherwise aren't
  recognised; agents' own commands aren't covered.

### 17. Audit log tampering
- **Threat:** editing/deleting/reordering audit events to hide actions.
- **Impact:** M. **Likelihood:** L.
- **Mitigation:** append-only JSONL with sha256 hash chain and sequence
  numbers; `audit verify` detects modification, deletion and reordering
  (`audit/mod.rs`); files `0600`; appends serialized with `flock`.
- **Residual:** chaining is **tamper-evident, not a signature**: someone
  with write access can rewrite the whole chain consistently or truncate
  the tail. `signature` is reserved for future signing.

### 18. State file corruption
- **Threat:** crashes or partial writes corrupt run/task state; a
  corrupted state causes wrong actions after restart.
- **Impact:** M. **Likelihood:** L.
- **Mitigation:** atomic temp-write + fsync + rename + dir fsync; sha256 in
  every envelope; corrupt files quarantined (`*.corrupt-<ts>`) and reported
  by `doctor`; schema versions with backed-up migrations; write-ahead
  intents so recovery distinguishes "never started" from "maybe happened"
  and marks ambiguity `needs_human` (`store/`, `recovery/`).
- **Residual:** a quarantined run document is lost to the engine (the
  worktree and audit remain for manual recovery).

### 19. Plugin supply-chain compromise
- **Threat:** malicious code in this plugin or its dependencies.
- **Impact:** critical. **Likelihood:** L.
- **Mitigation:** single Rust binary built locally from source with
  `cargo build --release --locked` (committed `Cargo.lock`); no
  download-and-execute, no postinstall; minimal manifest; Herdr shows an
  install preview; moderate dependency set reviewed at release.
- **Residual:** crates.io dependencies are trusted at their locked
  versions; users should pin `--ref` when installing.

### 20. Unsafe auto-updates
- **Threat:** silent updates replace trusted code.
- **Impact:** critical. **Likelihood:** L.
- **Mitigation:** the plugin has **no** self-update or update check.
  Updates happen only when the user reinstalls (`herdr plugin install …`),
  which rebuilds from the fetched source with the locked dependencies.
- **Residual:** reinstalling without pinning `--ref` takes the current
  default branch.

---

### 21. Agent games its own tests

- **Vector:** to make a check pass, an agent weakens or rewrites the tests
  it is judged by.
- **Mitigation:** default rules for deleted tests, test-runner config and
  added skips (0.2.0); with `contract-first`, the tests are written first,
  must fail on the base, are approved by a human and locked (diff-gate hash
  check for every agent, real-time refusal for Claude's file tools); the PR
  carries a receipt that `receipt verify` re-checks.
- **Residual:** a shell edit to a contract file is caught at the next diff
  gate, not while it runs; a weak contract passes weak code (the approver
  sees the tests and the red output; mutation checks are not implemented).

### 22. Hostile text from integrations

- **Vector:** GitHub issue bodies (`tracker import`, `task create
  --from-issue`), PR review comments (`run followup`) and Sentry error
  messages (`incidents task`) carry attacker-controlled text into prompts.
- **Mitigation:** treated as untrusted: prompt-only, never argv or env;
  Sentry data minimised to the exception and in-app frames and redacted;
  the resulting changes go through the same policy, review and approval as
  any task.

## Known gaps (tracked)

- No OS sandbox (`ExecutionSandbox` only `none`).
- No real-time interception of agent tool calls except Claude's
  `PreToolUse` hook (DENY rules only; Herdr offers nothing generic).
- Pane agents inherit Herdr's environment.
- Audit not signed.
