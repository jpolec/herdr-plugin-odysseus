# Policy engine

Every action the orchestrator controls, and every file change it finds in a
worktree, is evaluated against policy with one of three outcomes:

| Decision | Effect |
| --- | --- |
| `allow` | proceed |
| `require_approval` | pause the run and ask a human (see WORKFLOWS.md, approvals) |
| `deny` | stop safely; the run becomes `blocked` |

All evaluations that matter are audited as `policy_evaluated` events with the
subject, decision, reason and matching rule ids.

**Enforcement boundary.** Policy is enforced *before* orchestrator-run
actions (commands, git, PR, agent launch) and *after* agent steps by
inspecting the worktree diff. Commands an agent runs inside its own terminal
are not intercepted in real time. This is not a sandbox. See
[SECURITY_MODEL.md](SECURITY_MODEL.md).

Source: `src/policies/mod.rs`, `src/policies/command.rs`,
`policies/default.yaml`.

## File format

```yaml
version: 1                        # required, must be 1
name: my-project                  # optional
default_decision: allow           # optional: allow | require_approval | deny
rules:
  - id: approve-payments          # required, unique within the file
    description: Payment code needs a second pair of eyes.
    match:
      paths: ["src/payments/**"]
      exclude_paths: ["src/payments/**/*_test.go"]
    actions: [write, delete]      # shorthand for match.actions (merged)
    decision: require_approval
    reason: payment processing
```

Unknown keys are rejected. A rule must have at least one criterion among
`actions`, `paths`, `commands`, `command_tags`, `runners`,
`min_deleted_files`, `min_deleted_lines`, `min_files_changed`,
`added_lines` — a rule that would match everything is refused. Unknown
`command_tags` and invalid `added_lines` regular expressions are refused.

### Criteria (`match:`)

| Criterion | Type | Matches when |
| --- | --- | --- |
| `actions` | list | the subject's action is in the list |
| `paths` | globs | the worktree-relative path matches any glob |
| `exclude_paths` | globs | the path matches none of these (exceptions to `paths`) |
| `commands` | wildcards | any pattern matches the normalized command text or any shell segment |
| `command_tags` | tags | the command carries any of the tags |
| `runners` | wildcards | runner name matches |
| `steps` | wildcards | workflow step id matches |
| `branches` | globs | run branch matches |
| `repos` | globs | repository path matches |
| `shell` | bool | the command does / does not run through a shell |
| `min_lines_changed` | int | file's `+lines` + `-lines` ≥ N |
| `min_deleted_files` | int | diff summary: deleted files ≥ N |
| `min_deleted_lines` | int | diff summary: deleted lines ≥ N |
| `min_files_changed` | int | diff summary: changed files ≥ N |
| `added_lines` | regexes | any line the change *adds* matches any expression |

Semantics:

- **AND across criteria, OR within a list.** If a criterion needs data the
  subject doesn't have (e.g. `paths` on a command), the rule doesn't match.
- **Most restrictive wins:** `deny > require_approval > allow` over all
  matching rules; every match is recorded.
- **No match →** the set's `default_decision`.
- **Globs** (paths, branches, repos) use `globset` with literal separators:
  `*` stays within one path segment, `**` crosses directories,
  `**/.env` also matches a top-level `.env`, `{a,b}` alternation works.
- **Wildcards** (commands, runners, steps): `*` matches any run of
  characters including spaces and `/`; `?` one character.
- **`added_lines`** uses Rust `regex` syntax (compiled into one set per
  rule) and is only available for `write` subjects at the diff gate: the
  added lines of each changed file (`git diff -U0 <base> -- <path>`, or the
  whole content of an untracked file; at most 20 000 lines of 1 KiB). They
  are read only when some rule in the effective set uses `added_lines`.
  Removed lines never match, so re-enabling a test is never flagged. A
  subject without content (e.g. `policy check --path`) does not match.

### Actions

| Action | Evaluated for |
| --- | --- |
| `read` | reserved for file-read subjects (the default policy denies credential stores) |
| `write`, `delete` | each changed path in a worktree diff |
| `command` | workflow `command`/`check` steps |
| `git_commit` | orchestrator commits |
| `git_push` | branch pushes |
| `github_pr` | pull request creation |
| `agent_start` | launching an agent (its kind + launch args are the command) |
| `worktree_create` | creating a run's worktree |
| `diff_summary` | the aggregate diff (counts only) |
| `network`, `env_mutation` | reserved; not produced by the engine today (not observable) |

## Command normalization

Before matching, commands are canonicalized so a rule matches intent rather
than spelling. It is *not* a shell parser; shell input is split
conservatively and every piece is evaluated.

- Leading `VAR=value` assignments and the wrappers `env`, `sudo`/`doas`
  (with their options), `nohup`, `time`, `command`, `exec`, `builtin`,
  `nice [-n N]`, `timeout [opts] DURATION` are stripped. `sudo`/`doas` add
  the `privilege_escalation` tag.
- `argv[0]` is reduced to its basename (`/usr/bin/git` → `git`).
- git global options are removed: `-C DIR`, `-c k=v`, `--git-dir[=…]`,
  `--work-tree[=…]`, `--namespace[=…]`, `--no-pager`, `-P`, `-p`,
  `--paginate`, `--bare`, `--no-replace-objects`, `--exec-path…`.
  `/usr/bin/git -C x -c a=b push -f` → `git push -f`.
- `sh|bash|zsh|dash|ksh|fish -c SCRIPT` (also clusters like `-lc`) and
  `shell: true` scripts are split on `&&`, `||`, `;`, `|`, `&` and newlines
  outside quotes; `$(…)` and backtick substitutions become extra segments.
  Each segment is normalized and tagged; the whole command also gets the
  `shell` tag.
- The text is whitespace-collapsed.

```bash
$ herdr-orchestrator policy check --command "cargo test && git push -f origin main" --shell
DENY  command: cargo test && git push -f origin main
normalized: cargo test && git push -f origin main | cargo test | git push -f origin main
tags: git, git_force_push, git_push, shell
```

### Command tags

| Tag | Emitted for |
| --- | --- |
| `shell` | shell scripts (`shell: true`, `sh -c`) |
| `git` | any git command |
| `git_push` | `git push …` |
| `git_force_push` | `git push` with `--force`, `-f` (also in clusters like `-uf`), `--force-with-lease*`, `--force-if-includes`, `--mirror`, `--delete`/`-d`, or a `+ref`/`:ref` refspec |
| `git_reset_hard` | `git reset --hard/--merge/--keep`, `git checkout -f/--force` |
| `git_clean_force` | `git clean` with `-f`/`--force` (clusters included) |
| `git_branch_force_delete` | `git branch -D` (or `--delete --force`) |
| `git_history_rewrite` | `filter-branch`, `filter-repo`, `rebase/commit` with `-i`/`--interactive`/`--amend` |
| `rm_recursive` | `rm -r/-R/--recursive` |
| `rm_dangerous_target` | recursive `rm` (or `--no-preserve-root`) targeting `/`, `/*`, `~`, `$HOME`, `.`, `..`, `*`, `.git`, `/etc*`, `/usr*`, `/bin*`, `/System*`, `/Users`, `/home` |
| `terraform_apply` | `terraform|tofu|terragrunt apply|import|state` |
| `terraform_destroy` | `… destroy` or `apply -destroy` |
| `kubectl_mutation` | `kubectl|oc apply/delete/create/replace/patch/scale/rollout/edit/drain/cordon/set/label/annotate`; `helm install/upgrade/uninstall/delete/rollback` |
| `kubectl_delete_namespace` | `kubectl delete ns|namespace|namespaces|namespace/x` |
| `production_target` | an infra/deploy command (kubectl, oc, helm, terraform, tofu, terragrunt, aws, gcloud, az, flyctl, vercel, heroku, ansible-playbook, or anything tagged `deploy`) with an argument containing the word `prod`, `production` or `prd` |
| `deploy` | program name containing `deploy`, or a `deploy`/`deploy:*` argument |
| `network` | `curl`, `wget`, `nc`, `ncat`, `scp`, `rsync`, `ssh`, `sftp`, `ftp`, `telnet` |
| `credential_access` | a non-git command whose argument mentions `.ssh/id_`, `.ssh/identity`, `.aws/credentials`, `.aws/config`, `.netrc`, `.config/gh/hosts.yml`, `.docker/config.json`, `.kube/config`, `.gnupg`, `.npmrc`, `.pypirc`, `.git-credentials`, `/etc/shadow`, `keychain` |
| `package_install` | `npm/pnpm/yarn/bun install|i|add|ci`, `pip/pip3/uv/poetry install|add`, `cargo install`, `brew/apt/apt-get/dnf/yum install|upgrade` |
| `privilege_escalation` | `sudo`/`doas` wrapper |

## Where policy is evaluated

| Point | Subject | Outcome handling |
| --- | --- | --- |
| Run preparation | `worktree_create` with branch and repo | deny → run `blocked` |
| Agent step, before launch | `agent_start`; command = agent kind + pane args (+ headless argv); runner, step, branch | deny → `blocked`; approval → ask |
| Command / check step, before execution | `command` (argv or shell script); step, branch, cwd, repo | deny → `blocked` (never retried); approval → ask, unless covered by a preceding approval step |
| After each agent step (`policy_check: true`) and in `type: policy` steps | every changed path as `write` or `delete` with `lines_changed`, runner, step, branch; then `diff_summary` with counts | any deny → run `blocked` (changes are not committed); approvals collected into one request listing the paths; approved paths are remembered per run by change fingerprint |
| Path containment (same gate) | every changed path is canonicalized; paths outside the worktree or escaping through a symlink | always denied, regardless of policy |
| Before commits | `git_commit` | deny → `blocked` |
| Before pushes (git step, PR step, `run pr`) | `git_push` with the push argv (so command tags apply too) | deny → `blocked`; approval → ask unless covered |
| Before PR creation | `github_pr` | deny → `blocked`; approval → ask unless covered |
| `run pr` (human CLI handoff) | `git_push`, `github_pr` | deny refuses; require_approval is satisfied by the human invoking the command (audited as `approval_granted`) |
| Claude `PreToolUse` hook (`guard.claude_hook`), before each tool call of a Claude agent | `Bash` → the command (as a shell script, *without* the `command` action, so `approve-shell-mode` does not apply; tag and text rules do); `Write`/`Edit`/`MultiEdit`/`NotebookEdit` → `write` of the path (worktree-relative when inside); `Read` → `read` of the path | deny → the call is blocked and the reason goes to the agent (`agent_tool_checked`); require_approval and allow → no decision: Claude's own permission mode and the diff gate handle it, so nothing is asked twice |
| Contract lock (runs with a locked contract) | at the diff gate: each contract file's current content against its locked SHA-256; in the Claude hook: `Write`/`Edit`/`MultiEdit`/`NotebookEdit` of a contract path (rule `contract-lock`, source `run`) | changed hash → DENY, run `blocked` before anything is committed or pushed; in the hook → the call is refused. The PR step re-checks the hashes before pushing |

Only non-`allow` decisions for individual files are recorded in the run (to
avoid thousands of "allow" entries); every other evaluation is recorded.

### Engine guards (not policy files)

Two checks produce REQUIRE_APPROVAL decisions with a synthetic rule, shown
in approvals and the audit like any other rule:

| Rule id (source) | When |
| --- | --- |
| `task-scope` (`task`) | the task declared `--scope` globs (or an accepted epic task has `scope`) and a changed path matches none of them (a rename counts if its old path matched). Approved paths are remembered by content fingerprint. |
| `guard-test-only-retry` (`builtin:guard`) | a failed `command`/`check` step sent feedback to an agent and the new attempt changed only files matching `guard.test_paths` (tests and test configuration). Denying fails the step. Off with `guard.test_only_retry: false`. |

A run also asks once when its reported usage exceeds `limits.max_tokens` or
`limits.max_cost_usd` (`budget_exceeded`); denying stops it as `blocked`.

### Risk levels in the inbox

`inbox` and `approval batch` rate each run from the rules that fired (plus
diff size, reviews, acceptance, retries, the watchdog and a contract; see
`src/engine/digest.rs`). Rule ids that make a run **high** risk:
`approve-migrations`, `approve-infra`, `approve-ci-cd`,
`approve-auth-security-config`,
`approve-agent-instructions-and-orchestrator-config`,
`approve-infra-commands`, `approve-history-rewrite`,
`approve-privilege-escalation`, `approve-large-deletes`,
`approve-large-line-deletes`. **Medium**: `approve-test-deletion`,
`approve-test-runner-config`, `approve-disabled-tests`,
`guard-test-only-retry`, `task-scope`, `approve-large-lockfile-changes`,
`approve-network-commands`, `approve-shell-mode`. Risk never changes a
decision: `approval batch` only approves workflow `approval` steps, never a
policy question.

## Default policy summary

`policies/default.yaml` (built in; `default_decision: allow`):

| Rule | Decision | Matches |
| --- | --- | --- |
| `deny-secret-files` | deny | write/delete of `.env`, `.env.*`, `*.env`, `secrets/`, `.secrets/`, `.ssh/`, `.aws/credentials`, `.netrc`, `.npmrc`, `.pypirc`, `.git-credentials`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.keystore`, `*.jks` — except `.env.example/.sample/.template/.dist` |
| `deny-read-credential-stores` | deny | read of `.ssh/`, `.aws/credentials`, `.netrc`, `.config/gh/hosts.yml`, `.git-credentials`, `.kube/config` |
| `deny-credential-commands` | deny | tag `credential_access` |
| `deny-force-push` | deny | tag `git_force_push` |
| `deny-destructive-git` | deny | tags `git_reset_hard`, `git_clean_force`, `git_branch_force_delete` |
| `deny-destructive-rm` | deny | tag `rm_dangerous_target` |
| `deny-terraform-destroy` | deny | tag `terraform_destroy` |
| `deny-kubectl-delete-namespace` | deny | tag `kubectl_delete_namespace` |
| `deny-production-targets` | deny | tag `production_target` |
| `deny-agent-permission-bypass` | deny | `agent_start` with `--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, `--yolo`, `bypassPermissions` |
| `approve-agent-instructions-and-orchestrator-config` | approval | write/delete of `.ai/herdr-orchestrator/**`, `.ai/skills/**`, `.claude/**`, `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `AGENT.md`, `GEMINI.md`, `.codex/**`, `.gemini/**`, `.cursor/**`, `.cursorrules`, `.windsurfrules`, `.clinerules`, `.github/copilot-instructions.md`, `.mcp.json` — agents must not rewrite the rules they run under |
| `approve-test-deletion` | approval | delete of `tests/`, `test/`, `__tests__/`, `spec/`, `*_test.*`, `*_spec.*`, `*.test.*`, `*.spec.*`, `test_*.py` (editing tests is allowed) |
| `approve-test-runner-config` | approval | write/delete of `pytest.ini`, `tox.ini`, `jest.config*`, `vitest.config*`, `karma.conf*`, `.mocharc*`, `phpunit.xml*`, `.nycrc*`, `.coveragerc`, `codecov.yml` |
| `approve-disabled-tests` | approval | `added_lines` that skip, ignore or focus tests: `#[ignore]`, `it/test/describe/context.skip/only/todo(`, `xit(`, `xtest(`, `xdescribe(`, `fdescribe(`, `@pytest.mark.skip/skipif/xfail`, `pytest.skip(`, `@unittest.skip*`, `self.skipTest(`, `t.Skip(`, `@Disabled`, `@Ignore`, `markTestSkipped(` |
| `approve-migrations` | approval | `migrations/`, `migrate/`, `*.sql`, `schema.prisma`, `alembic/` |
| `approve-infra` | approval | `*.tf`, `*.tfvars`, `terraform/`, `k8s/`, `kubernetes/`, `helm/`, `charts/`, `Dockerfile*`, `docker-compose*.y(a)ml`, `compose*.yaml`, `ansible/`, `pulumi/`, `serverless.yml` |
| `approve-ci-cd` | approval | `.github/workflows/`, `.github/actions/`, `.gitlab-ci.yml`, `.circleci/`, `Jenkinsfile`, `azure-pipelines.yml`, `.buildkite/`, `CODEOWNERS` |
| `approve-auth-security-config` | approval | config files (`yml/yaml/json/toml/ini/conf`) under `auth|security|iam|rbac` dirs or with those words (or `permissions`, `policy`) in the name; `.gitattributes`, `.gitmodules` |
| `approve-large-lockfile-changes` | approval | writes to common lockfiles with ≥ 500 changed lines |
| `approve-large-deletes` | approval | diff with ≥ 50 deleted files |
| `approve-large-line-deletes` | approval | diff with ≥ 3000 deleted lines |
| `approve-remote-push` | approval | action `git_push` |
| `approve-push-commands` | approval | tag `git_push` |
| `approve-pr-creation` | approval | action `github_pr` |
| `approve-infra-commands` | approval | tags `terraform_apply`, `kubectl_mutation`, `deploy` |
| `approve-history-rewrite` | approval | tag `git_history_rewrite` |
| `approve-network-commands` | approval | tag `network` |
| `approve-privilege-escalation` | approval | tag `privilege_escalation` |
| `approve-shell-mode` | approval | `command` subjects with `shell: true` |

Everything else — ordinary source edits, test and lint commands, local
commits — is allowed.

## Checking a policy

```bash
herdr-orchestrator policy check --command "git -C . push -f origin main"   # DENY, exit 3
herdr-orchestrator policy check --path .env.local --action write            # DENY, exit 3
herdr-orchestrator policy check --path db/migrations/002.sql                 # REQUIRE_APPROVAL, exit 2
herdr-orchestrator policy check --path Cargo.lock --lines 800                # REQUIRE_APPROVAL, exit 2
herdr-orchestrator policy check --command "cargo test --all"                 # ALLOW, exit 0
herdr-orchestrator policy check --action github_pr                           # REQUIRE_APPROVAL
herdr-orchestrator policy check --command "claude --dangerously-skip-permissions" --action agent_start   # DENY
```

Exit codes: `0` allow, `2` require approval, `3` deny (`1` on usage
errors). `--json` prints the decision and the normalized command.
`--shell` treats the command as a shell script; `--runner`, `--branch` and
`--lines` fill the corresponding subject fields.

```bash
herdr-orchestrator policy show                 # sources, default decision, rule texts
herdr-orchestrator policy validate             # the effective set
herdr-orchestrator policy validate my.yaml     # specific files
```

## Adding project policy

1. Write `.ai/herdr-orchestrator/policy.yaml` in the repository.
2. Reference it from `.ai/herdr-orchestrator/config.yaml` (project
   `policy.files` are relative to the repository root; global ones to the
   global config directory):

```yaml
policy:
  files:
    - .ai/herdr-orchestrator/policy.yaml
```

Layering rules:

- The built-in default policy is included unless
  `policy.builtin_default: false`.
- `policy.files` from the global and project layers are **concatenated**:
  a project can add rules but cannot silently drop global ones.
- Because the most restrictive matching rule wins, an added rule can only
  make things stricter for the subjects it matches (an explicit `allow`
  does not override another rule's `deny`).
- `default_decision` can only be tightened by later files, never loosened.
- Duplicate rule ids within one file are rejected.
- Escape hatch: `policy.replace_global: true` in the project config drops
  the global `policy.files` (the built-in default still applies unless also
  disabled). Use deliberately; the effective sources are visible in
  `policy show` and `doctor`.
