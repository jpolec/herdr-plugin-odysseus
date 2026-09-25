# Runners

The engine drives every agent through one provider-neutral trait; provider
details live in *runner profiles*. Source: `src/runners/`.

## The `AgentRunner` trait

```rust
pub trait AgentRunner: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> RunnerCapabilities;   // interactive, reports_usage, needs_herdr, resumable_session
    /// Launch (or reattach to) the agent. Idempotent per step execution.
    fn start(&self, req: &AgentRequest, cancel: &CancelToken, events: &mut dyn FnMut(AgentEvent)) -> Result<AgentBinding>;
    /// Submit the prompt and wait until the agent settles, is cancelled, times out or disappears.
    fn send_and_wait(&self, req, binding, deadline, cancel, events) -> Result<AgentOutcome>;
    /// Wait on an agent whose prompt was already sent (recovery path).
    fn attach(&self, req, binding, deadline, cancel, events) -> Result<AgentOutcome>;  // default: unsupported
    fn interrupt(&self, binding: &AgentBinding) -> Result<()>;
    fn recover(&self, binding: &AgentBinding) -> RecoveryAssessment;           // Alive(status) | Gone | Unknown
    fn close(&self, binding: &AgentBinding) -> Result<()>;                     // default: no-op
    fn run(&self, req, cancel, events) -> Result<AgentOutcome>;                // start + send_and_wait
}
```

`AgentEvent`s (`Started`, `PromptSending`, `PromptSent`, `Blocked`,
`Unblocked`) let the engine persist and audit each transition as it
happens. `PromptSending` is persisted *before* the prompt goes out
(write-ahead), so recovery knows the prompt "may have been sent".
`AgentOutcome.end` is one of `Completed`, `Failed`, `TimedOut`, `Cancelled`,
`Lost` (pane/process vanished) or `Stalled` (prompt possibly not delivered).

## Modes

| Mode | What it does | Needs Herdr | Usage |
| --- | --- | --- | --- |
| `pane` | Interactive agent CLI in a real Herdr pane; a human can watch and type | yes | `unknown` |
| `headless` | Provider's non-interactive CLI as a child process | no | parsed when the provider reports it |
| `shell` | Any argv as a child process (aider, scripts) | no | `unknown` |
| `fake` | Deterministic in-process fake for tests/CI | no | fixed, marked `reported` |

If a `pane` runner is needed but Herdr is unreachable (or `herdr.mode:
disabled`), the runner falls back to its headless command when it has one.
With `herdr.mode: required` it fails instead. Runners without a headless
command (e.g. `copilot`) fail with a clear error.

## Built-in profiles

| Runner | Mode | Pane args | Headless argv | Usage parser | Extra env inherited |
| --- | --- | --- | --- | --- | --- |
| `claude` | pane | `--permission-mode acceptEdits` | `claude -p --output-format json --permission-mode acceptEdits` (prompt on stdin) | `claude-json` | `ANTHROPIC_*`, `CLAUDE_*` |
| `codex` | pane | `--sandbox workspace-write --ask-for-approval on-request` | `codex exec --json --sandbox workspace-write --skip-git-repo-check --output-last-message {{output_file}} -` | `codex-jsonl` | `OPENAI_*`, `CODEX_*` |
| `opencode` | pane | — | `opencode run {{prompt}}` | none | — |
| `gemini` | pane | — | `gemini -p {{prompt}}` | none | `GEMINI_*`, `GOOGLE_*` |
| `copilot` | pane | — | — | — | — |
| any other Herdr agent kind (`pi`, `cursor`, `devin`, `agy`, `cline`, `omp`, `mastracode`, `kimi`, `kiro`, `droid`, `amp`, `grok`, `hermes`, `kilo`, `qodercli`, `qwen`, `letta`, `maki`, `muse`) | pane | — | — | — | — |
| `shell` | shell | — | must be configured | none | — |
| `fake-<scenario>` | fake | — | — | fixed | — |

Claude (pane and headless) additionally gets
`--settings <worktree>/.herdr-orchestrator/claude-settings.json`, which
registers the orchestrator's `PreToolUse` policy hook
(`guard.claude_hook`, default on; see SECURITY_MODEL.md).

The permission modes are deliberately conservative: Claude may edit files
but asks before running shell commands; Codex writes inside its workspace
sandbox and asks before leaving it. Those questions show up as Herdr
`blocked` state and are answered by a human in the pane. The default policy
refuses to launch agents with all prompts disabled
(`--dangerously-skip-permissions`, `--yolo`, …).

### Fake scenarios

| Scenario | Behavior |
| --- | --- |
| `success` | writes `fake/<step>.txt` and a summary |
| `noop` | writes a summary only |
| `fail` | fails |
| `timeout` | hangs until the deadline or cancel |
| `crash` | agent is lost |
| `fix-on-retry` | writes `src/fake.rs`; from attempt 2 also writes `FIXED` |
| `review-findings` | review `changes_requested` with one high finding |
| `review-approve` | review `approved` |
| `review-fix` | `changes_requested` on attempt 1, `approved` afterwards |
| `review-invalid` | writes non-JSON output |
| `touch-secret` | writes `.env` (policy deny) |
| `touch-migration` | writes `db/migrations/001_init.sql` (policy approval) |
| `skip-test` | adds `#[ignore]` to a test in `src/lib.rs` (`approve-disabled-tests`) |
| `tests-only-on-retry` | attempt 1 changes code, later attempts only `tests/fake_test.rs` (`guard-test-only-retry`) |
| `touch-agent-config` | writes `CLAUDE.md` (agent instructions rule) |
| `plan` | writes a valid three-task plan (T2 depends on T1) |
| `plan-invalid` | writes a cyclic plan on every attempt |
| `plan-fix` | cyclic plan on attempt 1, valid afterwards |
| `plan-writes` | valid plan, but also writes `PLAN.md` (read-only violation) |
| `acceptance-met`, `acceptance-unmet` | judges criterion 1 `met` / `not_met` |
| `acceptance-fix` | `not_met` on attempt 1, `met` afterwards |
| `conformance` | one covered and one missing point, one follow-up |

`cargo test` and CI use these, so no provider account is needed.

## Pane flow

For each agent step execution:

1. **Reuse or create.** If the previous attempt of this step left a live
   agent that is `idle`/`done`, it is reused (it keeps its context).
   Otherwise `tab.create` in the run's Herdr workspace with the worktree as
   cwd and `HERDR_ORCH_*` variables in the pane environment.
2. **Label.** `pane.rename` to `#12 implement · codex`, and
   `pane.report_metadata` (source `plugin:jpolec.herdr-orchestrator`, tokens
   `orch_run`, `orch_step`). The binding is persisted before launch.
3. **Launch.** `agent.start {name, kind, pane_id, args}`. A new pane
   answers `agent_pane_busy` until its shell owns the foreground; the runner
   retries every 300 ms for up to `min(limits.agent_startup_timeout, 30s)`.
   `agent_not_ready` (e.g. blocked on a startup dialog) is handled like a
   block (step 5).
4. **Ready check.** Wait for `idle`/`done`, then for Herdr's
   `interactive_ready` flag (up to 10 s).
5. **Prompt.** `agent.prompt` with a server-side wait for
   `idle|done|blocked` (10 s chunks). A reused agent gets the compact
   follow-up prompt (step prompt with `{{feedback}}` + orchestrator
   instructions, no skill); a fresh agent gets skill + prompt +
   instructions.
6. **Wait.** Chunked `agent.wait` (10 s) until the agent settles. Between
   chunks the runner checks cancellation and the step deadline.
7. **Blocked.** Herdr's `blocked` means the agent is asking its human
   something (permission prompt, trust dialog, question). The step becomes
   `awaiting_human`, a Herdr notification is shown, and the runner keeps
   waiting. **It never answers on the human's behalf.** After the block
   clears the runner waits 1.5 s and re-confirms `idle` before sending
   anything (input sent while a dialog closes can be swallowed).
8. **Result.** The output file is read (see WORKFLOWS.md, output-file
   contract); the pane tail (`pane.read recent-unwrapped`,
   `output.pane_read_lines`, default 200) is kept as a redacted transcript in
   the step log.

Edge cases:

- `agent_prompt_stalled` (Herdr saw no activity within 5 s): if the agent is
  now working or blocked the wait continues; otherwise the step ends
  `Stalled` and the run becomes `needs_human`. The prompt is **not resent**
  automatically — it may or may not have arrived.
- `agent_blocked` on submit: wait for the human, then submit.
- Agent or pane gone: `Lost` with "agent process exited in its pane" or
  "agent pane was closed"; the step fails (and may be retried by
  `on_failure`).
- Timeout / cancel: the runner sends the interrupt keys (`esc` for
  `claude`/`codex`, `ctrl+c` otherwise; timeouts only when
  `herdr.interrupt_on_timeout`, default true). The pane is kept for
  inspection. `herdr.close_panes_on_success` closes panes of succeeded runs.
- Pane agents run with the environment of the Herdr pane's shell (Herdr's
  own environment plus `HERDR_ORCH_*`), not the orchestrator's
  `environment.inherit` allowlist. The allowlist applies to commands and to
  headless/shell runners.

Agent names follow Herdr's rule `[a-z][a-z0-9_-]{0,31}`:
`o{task}{variant}-{step}` lowercased, other characters replaced by `-`,
truncated to 26 characters, plus `-` and the last 4 characters of the
execution id, e.g. `o124b-implement-7d2e`.

Budgets: `limits.max_agents_per_run` counts distinct pane agents in a run;
`scheduler.max_parallel_agents` is a global limit across runs (agent steps
wait for a slot).

### Known E2E finding: Claude's folder-trust prompt

On first launch in a folder Claude Code has not seen, it asks "Is this a
project you trust?". Each run has a new worktree, so this dialog can appear
on every run and the step waits (`awaiting_human`) until someone answers in
the pane. Worktrees live under the repository
(`<repo>/.herdr-orchestrator/worktrees/`), so trusting the main repository
folder in Claude beforehand avoids the prompt.

## Headless and shell runners

- The prompt is written to `<output_file>.prompt.md` and sent on stdin.
- `HERDR_ORCH_PROMPT_FILE` and `HERDR_ORCH_OUTPUT_FILE` are set.
- Placeholders are substituted **as whole argv elements only**:
  `{{prompt}}`, `{{prompt_file}}`, `{{output_file}}`. A prompt starting with
  `-` is prefixed with a space so it cannot become an option. `argv[0]` may
  not be a placeholder.
- Output streams, redacted, to `logs/<run>/<exec>.log`. A non-zero exit fails
  the step.
- After a restart a headless agent is reported `Gone` and never re-run
  automatically.

### Usage provenance

| Parser | Source | Recorded |
| --- | --- | --- |
| `claude-json` | final JSON object | input = `input_tokens` + `cache_creation_input_tokens`; output = `output_tokens`; cached = `cache_read_input_tokens`; cost = `total_cost_usd`; source `reported` |
| `codex-jsonl` | `turn.completed` events | summed input/output/cached tokens; cost not reported (left empty rather than estimated); source `reported` |
| pane agents (Claude, Codex) | the agent's own session log, for the step's time window | Claude: `message.usage` of assistant lines, de-duplicated by message id, subagent transcripts included; Codex: growth of the cumulative `token_count` totals. Source `reported` when Herdr gave the native session id, `estimated` when the log was found by the worktree path. Cost stays empty (logs carry no price). |
| other pane agents | — | `unknown` (runtime only) |
| commands | — | runtime, source `measured` |

Totals keep the weakest provenance: mixing reported and unknown values
yields `estimated` (a lower bound). The UI shows "$1.84 reported" versus
"~$1.84 estimated", and token columns show `~` for estimates and `–` for
unknown. `herdr-orchestrator usage` sums agent usage per task.

Session logs: Claude `$CLAUDE_CONFIG_DIR` or `~/.claude`, file
`projects/<slug>/<session_id>.jsonl` (plus `<session_id>/subagents/*.jsonl`);
Codex `$CODEX_HOME` or `~/.codex`, `sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`
(the last eight day directories are searched). Pane agents keep one session
across retries, so each execution only counts lines between its start and
end. Read-only and local; off with `usage.session_logs: false`.

`limits.max_tokens` / `max_cost_usd` are enforced on known numbers: after an
agent step whose run total exceeds them, the run asks once whether to
continue (`budget_exceeded`; deny → `blocked`). Unknown usage never counts
as under budget, it just cannot be enforced.

## Configuring runners

Overrides go under `runners:` in global or project config. Fields: `mode`
(`pane`, `headless`, `shell`), `kind` (Herdr agent kind for pane mode),
`args` (pane launch args), `command` (headless/shell argv), `env_inherit`
(extra variable names or `PREFIX_*` patterns).

```yaml
runners:
  # Always run Claude headless (e.g. on a server without Herdr).
  claude:
    mode: headless

  # Codex with a different approval mode in its pane.
  codex:
    args: ["--sandbox", "workspace-write", "--ask-for-approval", "untrusted"]

  # A custom runner: aider via shell mode.
  aider:
    mode: shell
    command: ["aider", "--yes-always", "--no-auto-commits", "--message-file", "{{prompt_file}}"]
    env_inherit: ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"]

  # Another Herdr-supported agent in a pane under a custom name.
  review-gemini:
    mode: pane
    kind: gemini
```

Validation: a pane runner's `kind` must be an agent kind Herdr can start; a
headless/shell runner needs a non-empty `command` whose program is not a
placeholder. Unknown runner names are accepted only when an override
defines them (with a `mode`).

```bash
herdr-orchestrator runners      # every runner, its mode, kind and whether its binary is on PATH
```
