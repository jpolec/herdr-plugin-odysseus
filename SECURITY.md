# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately** through GitHub Security
Advisories on this repository ("Security" → "Report a vulnerability").
Do not open public issues for security problems.

Include the version (`herdr-orchestrator --version`), Herdr version, OS,
and reproduction steps. We aim to acknowledge reports within 7 days. There
is no bug bounty.

## Supported versions

| Version | Supported |
| --- | --- |
| 0.1.x | ✅ |

## Scope

In scope: the `herdr-orchestrator` binary, its manifest, shipped workflows,
policies and skills — e.g. policy bypasses for orchestrator-run commands,
command/argument injection, path or symlink escapes the diff gate misses,
secrets written to state/audit/logs unredacted, audit-chain verification
flaws, state corruption leading to unsafe actions.

Out of scope: actions performed *inside* agent CLIs (Claude, Codex, …) which
this plugin documents it cannot intercept; vulnerabilities in Herdr, agent
CLIs, `git` or `gh`; running the plugin on repositories whose configuration
you do not trust.

## Documentation

- [docs/SECURITY_MODEL.md](docs/SECURITY_MODEL.md) — what is enforced, what
  is only observed, and why this is not a sandbox.
- [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) — threats, mitigations and
  residual risk.

## Telemetry

None. No telemetry, analytics, crash reporting or update checks. All state
stays on your machine.
