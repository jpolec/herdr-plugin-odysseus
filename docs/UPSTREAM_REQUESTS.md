# Upstream feature requests for Herdr

These are gaps found while building herdr-orchestrator against Herdr 0.9.0.
For each one: the problem, the workaround in this plugin, and a proposed API.
All workarounds are isolated in `src/herdr/`, `src/daemon/` or
`src/runners/pane.rs`.

## 1. Supervised long-running plugin processes

- **Problem:** Startup hooks are one-shot and nothing supervises a plugin process. Orchestration has to outlive panes, clients and plugin reloads.
- **Workaround:** A self-spawned daemon (`setsid`, `flock` single instance), started on demand by the CLI, TUI and startup hook. It idle-exits, and recovery runs on every start.
- **Proposal:** A `[[services]]` manifest entry (`command`, `restart = "on-failure"|"always"`, `stop_signal`, `stop_timeout_ms`) with Herdr-owned lifecycle, logs in `plugin.log.list`, and a `plugin.service.status` method. Stop it on disable, uninstall or server stop, and hand it over during live handoff.

## 2. Plugin-scoped durable storage (optional)

- **Problem:** There is no storage API. Every plugin reinvents atomic writes, locks and migrations.
- **Workaround:** Checksummed JSON envelopes, `flock`, a migration registry (`src/store/`).
- **Proposal:** A small transactional KV (`plugin.kv.get/put/cas/list`) scoped by plugin id with compare-and-swap. Keep it optional, because files remain fine for audit logs.

## 3. Agent turn identifiers / per-prompt completion

- **Problem:** `agent.wait` "does not track turns". If the agent is already working, completing that turn can satisfy the wait. `agent_prompt_stalled` can't say whether the text reached the agent.
- **Workaround:** Wait for `interactive_ready`, add a settle delay after a `blocked → idle` transition, and wait atomically through `agent.prompt` + `wait`. A stall becomes `needs_human` and is never resent blindly.
- **Proposal:** `agent.prompt` returns `turn_id`, and `agent.wait {turn_id}` completes only when that turn ends. Add an event `pane.agent_turn_completed {turn_id, outcome}` plus a `delivered: bool` in the prompt response.

## 4. Pre-execution hook for agent tool calls

- **Problem:** Commands and file writes an agent performs inside its own process can't be intercepted, so policy is only enforceable after the fact (diff at step boundaries).
- **Workaround:** Conservative launch permission modes, cwd confined to the worktree, a diff-based policy gate, and a policy check on launch flags.
- **Proposal:** A Herdr-mediated approval channel. When an integration detects a tool-use request (Herdr already detects `blocked` UIs, and several agents expose hook systems), emit `pane.agent_permission_requested {tool, argv, path}` and accept `agent.permission.respond {allow|deny}`, so a plugin can apply policy before execution. It should be opt-in per pane.

## 5. Event hooks filtered by pane ownership

- **Problem:** Hooking `pane.agent_status_changed` spawns the plugin for every agent in every session.
- **Workaround:** Not hooked. Drivers block on server-side `agent.wait` instead.
- **Proposal:** Filters on `[[events]]` such as `owned_by = "self"`, `pane_ids`, `agent_status = [...]`, so hooks fire only for panes the plugin created (via `tab.create`/`agent.start` with a `owner = plugin:<id>` param).

## 6. Opening plugin panes with arguments

- **Problem:** `plugin.pane.open` launches a fixed manifest command. Passing a view or run id needs env (`HERDR_ORCH_VIEW`) or an extra manifest entry.
- **Workaround:** `env` on `plugin.pane.open`, read at startup.
- **Proposal:** An `args: [..]` field on `plugin.pane.open`, appended to the manifest argv and validated against an allowlist declared in the manifest (`accepts_args = true`).

## 7. Plugin-owned pane metadata restored across restarts

- **Problem:** Pane metadata tokens are not restored after a server restart, and PTYs die on a cold restart. A plugin has to keep its own `run → step → pane → agent_session` map.
- **Workaround:** Our run state is authoritative. Recovery reconciles with `agent.get`, and agents that are gone after being prompted become `needs_human`.
- **Proposal:** Persist `plugin:`-sourced metadata tokens with the session. Add `pane.owner` (plugin id) to `PaneInfo`, and provide an `agent.resume {agent_session}` helper to relaunch a native session (claude/codex session id) in a new pane after a cold restart.
