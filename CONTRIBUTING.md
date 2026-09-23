# Contributing

## Setup

```bash
rustup toolchain install stable     # or Homebrew rust
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

Link your checkout into Herdr for manual testing:
`cargo build --release && herdr plugin link .`. Remember that plugin
registration is global to your user, across all Herdr sessions.

## Layout

`src/model.rs` holds the domain model and state machines. The modules are:

- `engine/`: driver, steps, scheduler, plan, handoff
- `runners/`: pane, headless, shell, fake, profiles
- `policies/`: rules, command normalization
- `workflow/`: DSL, templates, catalog
- `herdr/`: socket client, adapters, mock
- `store/`: persistence
- `audit/`, `recovery/`, `daemon/`, `git/`, `github/`, `checks/`, `security/`, `telemetry/`
- `cli/`, `ui/`

Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Tests

- **Unit tests** live next to the code (`#[cfg(test)]`).
- **Integration tests** are in `tests/engine.rs`, with the harness in `tests/common/mod.rs`. They use real `git` in temp repos, fake runners, `MockHerdr` and a fake `gh` script.
- **No network in tests.** Nothing may call real agents, GitHub or a live Herdr. Use:
  - `fake-*` runners: `success`, `fail`, `timeout`, `crash`, `noop`, `fix-on-retry`, `review-findings`, `review-approve`, `review-fix`, `review-invalid`, `touch-secret`, `touch-migration`
  - `MockHerdr` behaviors: `Finish`, `Block`, `Hang`, `Crash`, `set_busy_starts`, `set_block_on_start`, `set_available`
- **E2E against a real Herdr is opt-in and manual.** Always use an isolated *named* session, never your default one. See [HERDR_INTEGRATION.md](docs/HERDR_INTEGRATION.md#end-to-end-testing).
- Avoid timing races in tests. Poll durable state with `Harness::until` instead of asserting right after a status change.

## Style

- Match the surrounding code, and keep the state machines explicit. Every status change goes through `set_status` / `set_exec_status`.
- Follow the write-ahead rule: persist intent before any side effect.
- Commands are argv arrays. Never build a shell string from untrusted text.
- Never claim stronger guarantees than the code enforces. Document limits.
- New dependencies need a justification in the PR, and must keep `Cargo.lock` committed.

## Adding things

- **A runner / provider:** add a built-in profile in `src/runners/profiles.rs` (Herdr kind, pane args, headless argv, usage format, env). Pane agents need a Herdr agent kind. If the provider reports usage, extend `headless::parse_usage` and mark it `Reported`, never guessed. Add tests with `MockHerdr`.
- **A step type:** add a `StepSpec` variant and its validation in `src/workflow/mod.rs`, an executor in `src/engine/steps.rs` that follows write-ahead, an audit event and the policy action, a recovery classification in `src/recovery/mod.rs`, a plan line in `engine/plan.rs`, and docs in `docs/WORKFLOWS.md`.
- **A policy command tag:** add it to `TAGS` and `classify` in `src/policies/command.rs`, cover evasions (wrappers, flag clusters, `sh -c`) in the tests, and reference it from `policies/default.yaml` if it should be on by default.
- **An audit event:** use `snake_case` names and redactable data. Never put environment values or prompts in the data; use hashes instead.

## Pull requests

Before opening a PR, run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test`. Update `CHANGELOG.md` and the relevant docs.
