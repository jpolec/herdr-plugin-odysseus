# Herdr integration

Everything Herdr-specific lives in `src/herdr/`. The engine only sees the
`HerdrApi` trait. The real implementation is `SocketHerdr`, split into
`pane_adapter.rs`, `agent_adapter.rs` and `worktree_adapter.rs`. `MockHerdr`
(`mock.rs`) is used by the tests.

## Verified facts (Herdr 0.9.0, protocol 22)

These were verified against `herdr api schema --json` from the installed binary
and against the Herdr source at 0.9.1.

| Fact | Consequence |
| --- | --- |
| Plugins are argv commands declared in `herdr-plugin.toml` (`[[build]]`, `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]`). No shell expansion. `min_herdr_version` is required. | One binary serves every entrypoint. |
| Startup hooks are one-shot, and there is no supervised plugin daemon. | We run our own single-instance daemon. |
| Env: `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ID/ROOT/CONFIG_DIR/STATE_DIR/CONTEXT_JSON`, `HERDR_PLUGIN_EVENT[_JSON]`, `HERDR_PLUGIN_ACTION_ID`, `HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_WORKSPACE_ID/TAB_ID/PANE_ID`. | Paths and context come from the environment. |
| No plugin storage API. | We own our files under the state directory. |
| NDJSON socket: `{"id","method","params"}` → `result` or `error{code,message}`. | Thin client; unknown fields are ignored. |
| `agent.start {name, kind, pane_id, args, timeout_ms}` needs an idle shell pane. Names must match `[a-z][a-z0-9_-]{0,31}`. | New tab per agent. Names are `o<task><variant>-<step>-<4hex>`. |
| `agent.prompt` accepts `wait:{until, timeout_ms}` (server-side). Errors include `agent_blocked`, `agent_prompt_stalled`, `timeout`, `agent_not_ready`. | Completion uses semantic state, not screen scraping. |
| `blocked` means the agent is showing its own approval or question UI. | The step becomes `awaiting_human`; we notify and never auto-answer. |
| Plugin `[[events]]` accept only a fixed list (`workspace.*`, `worktree.*`, `tab.*`, `pane.created/closed/focused/moved/exited/agent_detected/agent_status_changed`). | We hook `pane.closed` and `worktree.removed` as wake-ups only. |
| Public pane ids persist across restarts, but PTYs don't survive a cold server restart. | Our run state is authoritative for recovery. |
| `worktree.create` may `remove_dir_all` a leftover checkout directory. | We create worktrees with `git`, then `worktree.open` them. |
| `prefix+o` is bound to `open_notification_target` by default. | We suggest `prefix+shift+o` and never edit your config. |

## Socket methods used

| Method | Where | Why |
| --- | --- | --- |
| `ping` | `agent_adapter.rs` (`HerdrApi::ping`), CLI, `doctor` | reachability and version |
| `worktree.open` | `worktree_adapter.rs` | open the run's git worktree as a Herdr workspace grouped with the repo |
| `workspace.create` | `worktree_adapter.rs` | fallback if `worktree.open` rejects the checkout |
| `tab.create` | `pane_adapter.rs` | one tab per agent step, cwd = worktree, with `HERDR_ORCH_*` env |
| `pane.rename` | `pane_adapter.rs` | label `#12 implement · codex` |
| `pane.report_metadata` | `pane_adapter.rs` | display title and tokens `orch_run`, `orch_step` (source `plugin:jpolec.herdr-orchestrator`) |
| `pane.get` | `pane_adapter.rs` | distinguish "agent exited" from "pane closed" |
| `pane.list` | `pane_adapter.rs` | available to recovery and diagnostics |
| `pane.read` (`recent-unwrapped`) | `pane_adapter.rs` | transcript fallback when the agent didn't write its output file |
| `pane.close` | `pane_adapter.rs` | `herdr.close_panes_on_success` |
| `pane.focus` | `pane_adapter.rs` | `run focus` and the TUI `f` key, as a fallback |
| `pane.process_info` | `pane_adapter.rs` | foreground process sampling (best effort) |
| `notification.show` | `pane_adapter.rs` | approvals, blocked agents, completions |
| `plugin.pane.open` | `pane_adapter.rs` | actions open the dashboard (overlay) or the new-task popup, with env `HERDR_ORCH_VIEW` |
| `agent.start` | `agent_adapter.rs` | launch the provider CLI in the pane (timeout clamped to 3001–300000 ms) |
| `agent.get` | `agent_adapter.rs` | status, `interactive_ready`, reuse checks, recovery |
| `agent.list` | `agent_adapter.rs` | diagnostics |
| `agent.prompt` + `wait` | `agent_adapter.rs` | submit the prompt and wait for the first settle atomically |
| `agent.wait` | `agent_adapter.rs` | chunked (10 s) waits for `idle/done/blocked`, so cancellation is observed |
| `agent.send_keys` | `agent_adapter.rs` | interrupt on cancel or timeout (`esc` for claude/codex, `ctrl+c` otherwise) |
| `agent.focus` | `agent_adapter.rs` | `run focus` and the TUI `f` key |

## Manifest walkthrough (`herdr-plugin.toml`)

- `[[build]]`: `cargo build --release --locked`. There is no other install step.
- `[[startup]]`: `hook startup` starts the daemon only if there is queued or unfinished work. The daemon runs recovery on start.
- `[[actions]]`:
  - `open` (`hook open`) opens the dashboard pane.
  - `new-task` (`hook new-task`) opens the popup form.
  - `approvals` (`hook open --view approvals`) opens the dashboard on the approvals view.
- `[[events]]`: `pane.closed` and `worktree.removed` run `hook event`. That forwards a nudge to a running daemon and **never** starts one.
- `[[panes]]`: `dashboard` (`ui`, overlay) and `new-task` (`ui --new-task`, popup 80%×70%).

Relative programs such as `./target/release/herdr-orchestrator` resolve against
the plugin root (Herdr's `plugin_command.rs`).

## Environment consumed

`HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_ROOT`,
`HERDR_PLUGIN_ID`, `HERDR_PLUGIN_CONTEXT_JSON` (the repo is taken from
`worktree.repo_root`, then `focused_pane_cwd`, then `workspace_cwd`),
`HERDR_PLUGIN_EVENT[_JSON]`, `HERDR_SOCKET_PATH`, `HERDR_SESSION`,
`HERDR_ORCH_VIEW`. Our own overrides are `HERDR_ORCH_STATE_DIR`,
`HERDR_ORCH_CONFIG_DIR`, `HERDR_ORCH_GH_BIN` and `HERDR_ORCH_LOG`.

**Socket resolution** mirrors Herdr's documented order. The config key
`herdr.socket` wins, then `HERDR_SOCKET_PATH`, then
`$XDG_CONFIG_HOME|~/.config/herdr/sessions/$HERDR_SESSION/herdr.sock`, then the
default `~/.config/herdr/herdr.sock`. Set `herdr.mode` to `auto` (default),
`required` or `disabled`.

**Path fallback.** When the CLI runs outside Herdr it has no
`HERDR_PLUGIN_STATE_DIR`. We then derive the same paths Herdr uses in
`plugin_paths.rs`:

- state: `${XDG_STATE_HOME:-~/.local/state}/herdr/plugins/jpolec.herdr-orchestrator`
- config: `${XDG_CONFIG_HOME:-~/.config}/herdr/plugins/config/jpolec.herdr-orchestrator`

This logic is isolated in `config::Paths::discover`. `doctor` warns if the
environment and the derived paths disagree.

The daemon control socket is `$STATE/daemon.sock`. If that path would exceed
the Unix socket length limit, it moves to `/tmp/herdr-orch-<uid>/<hash>.sock`
(mode 0700, owner-checked).

## Why worktrees are created with git

`git::add_worktree` records the base SHA before anything else happens. It
validates the branch name and refuses collisions, and it is idempotent: the
same path and branch return `Existing`. Herdr's `worktree.create` may
`remove_dir_all` a leftover directory at the target path, which is a
destructive side effect we don't want. So we create the checkout with `git`
ourselves, then call `worktree.open --path` so Herdr groups the workspace with
its parent repository.

## Why a daemon

Herdr plugin v1 has no supervised background processes, and runs must survive
the pane, the client and the plugin closing. A detached `daemon run`
(`setsid`, single instance through `flock`) hosts the scheduler. It is started
on demand by the CLI, the TUI and the startup hook. It exits after 30 idle
minutes, and never exits while approvals are pending. See
[UPSTREAM_REQUESTS.md](UPSTREAM_REQUESTS.md).

## Event hooks

We hook only `pane.closed` and `worktree.removed`, and only to wake the daemon.
We deliberately do **not** hook `pane.agent_status_changed`. It fires for every
agent in every session and would spawn our binary on each change. Run drivers
already block on Herdr's server-side, event-driven `agent.wait`.

## End-to-end testing

Never test against your default session. Use an isolated named session:

```bash
env -u HERDR_SOCKET_PATH HERDR_SESSION=orch-e2e herdr --session orch-e2e server &   # headless
SOCK=~/.config/herdr/sessions/orch-e2e/herdr.sock
mkdir -p /tmp/e2e/cfg && printf "herdr:\n  mode: required\n  socket: $SOCK\n" > /tmp/e2e/cfg/config.yaml
cd /tmp/e2e/repo   # a throwaway git repo
env -u HERDR_SOCKET_PATH -u HERDR_SESSION \
  HERDR_ORCH_STATE_DIR=/tmp/e2e/state HERDR_ORCH_CONFIG_DIR=/tmp/e2e/cfg \
  herdr-orchestrator task create "Create hello.txt containing: hi" --workflow quick-task --runner claude --foreground
HERDR_SOCKET_PATH=$SOCK herdr agent list           # watch / answer agent prompts
herdr --session orch-e2e server stop
```

In a folder it hasn't seen, Claude Code first asks whether you trust the
folder. Herdr reports that as `blocked`, the step shows `awaiting_human`, and
**you** answer it in the pane. Placing worktrees inside the repository means
trusting the repository once usually covers them.

The first real E2E run found two bugs that the mock could not reproduce. Both
are now fixed and have regression coverage.

1. **`agent_pane_busy` right after `tab.create`.** The new pane's shell is not the foreground owner yet. `PaneRunner::start` now retries `agent.start` on `agent_pane_busy` (300 ms steps, up to 30 s). `MockHerdr::set_busy_starts` reproduces it.
2. **Prompt swallowed right after the trust dialog.** The status turned `idle` a moment before Claude's input box accepted text, so the prompt vanished. Herdr correctly returned `agent_prompt_stalled`, and we correctly went to `needs_human` without resending. The runner now waits for `interactive_ready` and, after any unblock, lets the UI settle for 1.5 s and re-checks the state before prompting.
