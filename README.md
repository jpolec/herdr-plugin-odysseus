# herdr-orchestrator (Odysseus)

**Governed multi-agent software-development workflows, native to [Herdr](https://github.com/herdrdev/herdr).**

Herdr already runs your coding agents in real terminal panes. `herdr-orchestrator`
adds the layer above it:

```text
task → workflow → isolated worktree → agent(s) → checks → retry with feedback
     → independent review → policy → human approval → draft PR → audit record
```

Agents run **interactively inside real Herdr panes** (`#12 implement · codex`),
so you can watch any of them, type into them, or take over at any time. The
orchestrator never scrapes screens for completion: it uses Herdr's semantic
agent state (`idle / working / blocked / done`). When an agent asks its human
something (`blocked`), the orchestrator tells you and waits. It never answers
for you.

## Features

- **Tasks and a local queue**: FIFO, `max_parallel_runs`, `max_parallel_agents`, pause/resume, cancel, retry.
- **One git worktree and branch per run** (`.herdr-orchestrator/worktrees/<task>-<slug>`, `herdr/<task>-<slug>`). Worktrees are never force-reset or force-pushed, and a dirty worktree is never deleted.
- **YAML workflows** with `agent`, `command`, `check`, `approval`, `policy`, `git` and `github_pr` steps, plus constrained `{{templates}}`.
- **Retry loops**: a failed check sends a bounded failure excerpt back to the implementer. With pane agents, the retry goes to the *same live agent*, which keeps its context.
- **Agent-to-agent handoff**: Codex implements, Claude reviews with validated structured findings, and findings route back for remediation (`gate: true`).
- **Policy engine** (ALLOW / REQUIRE_APPROVAL / DENY):
  - pre-flight checks on orchestrator commands
  - a diff-based gate on everything agents changed
  - checks on agent launch flags

  It ships with a conservative default policy: secrets denied, migrations/infra/CI need approval, force-push/`reset --hard`/`terraform destroy` denied, and more.
- **Human approvals with full context**: changed files, diff stat, checks, policy reasons and the exact action that will happen.
- **Tamper-evident audit trail**: append-only JSONL per run, SHA-256 hash chained, secrets redacted, `audit verify`.
- **Variants (tournament mode)**: `--variants 3 --variant-runners codex,codex,claude`, side-by-side `task compare`, explicit human `task select`. No opaque "best" score.
- **Durable and recoverable**: write-ahead state, a single-instance daemon, and reconciliation after crashes. Nothing ambiguous is re-run blindly.
- **Provider-neutral runners**: `claude`, `codex`, `opencode`, `gemini`, any Herdr agent kind, headless mode, arbitrary `shell` agents, and deterministic `fake-*` runners for CI.
- **TUI pane, CLI, dry-run planning and `doctor`.**
- **Local-first**: no accounts, no SaaS, **no telemetry**. It reuses your logged-in `claude`/`codex`/`gh`.

## Requirements

- Herdr **≥ 0.9.0** on macOS or Linux (Windows is not supported: the socket client is Unix-only)
- Rust stable (to build; `cargo build --release --locked` runs at install)
- `git`
- Optional: `gh` (PRs, issues), `claude`, `codex`, `opencode` (runners)

## Install

```bash
herdr plugin install jpolec/herdr-plugin-odysseus            # GitHub shorthand
herdr plugin install jpolec/herdr-plugin-odysseus --ref v0.1.0   # pin a revision
```

The repository is `jpolec/herdr-plugin-odysseus`; the plugin id is
`jpolec.herdr-orchestrator` and the binary is `herdr-orchestrator`. Herdr shows a
trust preview of the manifest and the build command before installing; review
`herdr-plugin.toml`, which is short.

Local development:

```bash
cargo build --release
herdr plugin link .
herdr plugin action list --plugin jpolec.herdr-orchestrator
```

### Keybinding (opt-in)

The plugin never edits your Herdr config. `prefix+o` is already Herdr's default
`open_notification_target`, so this suggests `prefix+alt+o`. Add to
`~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+alt+o"
type = "plugin_action"
command = "jpolec.herdr-orchestrator.open"
description = "orchestrator"
```

Other actions: `jpolec.herdr-orchestrator.new-task` (popup form) and
`jpolec.herdr-orchestrator.approvals`.

## Quick start

```bash
cd your-repo

# See what would happen (policy is evaluated, nothing is created)
herdr-orchestrator plan "Add a --json flag to the export command"
herdr-orchestrator --dry-run task create "Add a --json flag" --workflow implement-review

# Queue it (starts the engine daemon on demand)
herdr-orchestrator task create "Add a --json flag to the export command"
herdr-orchestrator run list
herdr-orchestrator run show '#1'

# Approvals
herdr-orchestrator approval list
herdr-orchestrator approval show ap-1a2b
herdr-orchestrator approval approve ap-1a2b --note "reviewed"

# Tournament mode
herdr-orchestrator task create "Speed up ingestion" --workflow variant-review \
  --variants 3 --variant-runners codex,codex,claude
herdr-orchestrator task compare 2
herdr-orchestrator task select 2 '#2B'
herdr-orchestrator run pr '#2B'          # draft PR for the chosen variant

# Integrity and health
herdr-orchestrator audit verify --all
herdr-orchestrator doctor

# CI / headless: no daemon, deterministic fake agents
herdr-orchestrator task create "smoke" --workflow quick-task --runner fake-success --foreground
```

Inside Herdr, the binary is `./target/release/herdr-orchestrator` in the plugin
directory. Put it on your `PATH` or alias it for CLI use.

Other commands include `run retry [--from-step S]`, `run cancel`, `run pause|resume`,
`run diff`, `run logs`, `run focus` (focuses the agent pane in Herdr),
`queue pause|resume|status`, `workflow list|show|validate`,
`policy check|validate|show`, `config show [--layers]|paths|init`, `runners` and
`skills`. Add `--json` for machine-readable output.

## The orchestrator pane (TUI)

| Screen | Keys |
| --- | --- |
| Dashboard | `n` new task · `enter` inspect · `a` approvals · `r` retry · `x` cancel · `d` diff · `f` focus agent pane · `p` pause queue · `q` quit |
| Run detail | `↑↓` step · `enter`/`l` log · `d` diff · `f` focus agent · `a` approval · `r` retry · `x` cancel · `esc` back |
| Approval | `y` approve once · `n` deny · `c` cancel run · `d` diff · `f` open agent pane · `esc` back |
| New task | `tab` next field · `←→` choose · `ctrl+s` create · `esc` cancel |

The pane only reads state and records decisions. Closing it never affects
running work.

## Configuration

Precedence (later wins): built-in defaults → global config
(`$HERDR_PLUGIN_CONFIG_DIR/config.yaml`) → project config
(`.ai/herdr-orchestrator/config.yaml`) → workflow defaults → task options → CLI
flags. `policy.files` concatenate across layers, so a project can add policy but
not silently drop global policy.

Run `herdr-orchestrator config init` for a commented project config, and see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §16. Project workflows and skills
live in `.ai/herdr-orchestrator/workflows/` and `.ai/herdr-orchestrator/skills/`
(also `.ai/skills/`).

State lives in `HERDR_PLUGIN_STATE_DIR` (by default
`~/.local/state/herdr/plugins/jpolec.herdr-orchestrator/`). Override it with
`HERDR_ORCH_STATE_DIR`. `herdr-orchestrator config paths` prints the locations.

## Safety: what this does and does not do

- Orchestrator-run commands (workflow commands, git, gh, agent launch arguments) are policy-checked **before** execution and audited.
- Agents' own tool use inside their panes is **not** intercepted in real time. The orchestrator does four things instead:
  - launches agents with conservative permission modes (`claude --permission-mode acceptEdits`, `codex --sandbox workspace-write --ask-for-approval on-request`)
  - confines them to the run's worktree as cwd
  - checks the **diff** against policy at every step boundary and before any approval or PR
  - blocks the run on violations
- **This is not a sandbox.** An agent can still run arbitrary commands as your user. The `ExecutionSandbox` seam exists for future OS-level isolation.
- Repository git hooks are disabled for orchestrator commits. Child processes get an allowlisted environment, and secrets are redacted from audit and logs (best-effort, pattern-based).
- Nothing auto-merges or deploys. PRs are drafts, and opening one requires approval by default.

Details: [SECURITY_MODEL](docs/SECURITY_MODEL.md) and [THREAT_MODEL](docs/THREAT_MODEL.md).

**No telemetry.** Nothing leaves your machine except what your agents, `git` and
`gh` do on your behalf.

## Testing

```bash
cargo test            # unit + integration; no accounts, no network, no Herdr needed
```

Integration tests use real `git` in temp repos, deterministic fake runners
(`fake-success`, `fake-fail`, `fake-timeout`, `fake-review-findings`,
`fake-fix-on-retry`, `fake-touch-secret`, …), a mock Herdr and a fake `gh`.
For real end-to-end testing against an isolated named Herdr session, see
[HERDR_INTEGRATION.md](docs/HERDR_INTEGRATION.md#end-to-end-testing).

## Documentation

- [ARCHITECTURE](docs/ARCHITECTURE.md): design and decisions
- [HERDR_INTEGRATION](docs/HERDR_INTEGRATION.md): verified Herdr API usage, manifest, E2E
- [WORKFLOWS](docs/WORKFLOWS.md): workflow DSL and templates
- [POLICY_ENGINE](docs/POLICY_ENGINE.md): rules, command normalization, default policy
- [RUNNERS](docs/RUNNERS.md): runner modes and profiles
- [AUDIT](docs/AUDIT.md): event schema, hash chain, redaction
- [RECOVERY](docs/RECOVERY.md): durability and restart behavior
- [SECURITY_MODEL](docs/SECURITY_MODEL.md) and [THREAT_MODEL](docs/THREAT_MODEL.md)
- [UPSTREAM_REQUESTS](docs/UPSTREAM_REQUESTS.md): Herdr features that would help
- [SECURITY.md](SECURITY.md), [CONTRIBUTING.md](CONTRIBUTING.md), [CHANGELOG.md](CHANGELOG.md)

## Acknowledgements

[open-mercato/cezar](https://github.com/open-mercato/cezar) (MIT) was used as a
**functional reference** for tasks, queues, worktrees, workflows, variants and
GitHub handoff. No Cezar code was copied. This is an independent Rust
implementation designed around Herdr panes.

## License

MIT. See [LICENSE](LICENSE).
