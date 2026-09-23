# Examples

`project/` shows what a repository using herdr-orchestrator typically adds.
Copy the `.ai/` directory into your repository root and adjust.

| File | Purpose |
| --- | --- |
| `project/.ai/herdr-orchestrator/config.yaml` | Project config (checks, runners, limits, policy files) |
| `project/.ai/herdr-orchestrator/policy.yaml` | Project policy rules layered on top of the built-in default |
| `project/.ai/herdr-orchestrator/workflows/codex-claude-remediate.yaml` | Codex implements → Claude reviews (gated) → Codex fixes findings |
| `project/.ai/skills/project-conventions.md` | A project skill usable from any agent step |

Try them without any agent account:

```sh
herdr-orchestrator --repo . workflow validate
herdr-orchestrator --repo . plan "Add pagination to /orders" -w codex-claude-remediate
herdr-orchestrator --repo . task create "Add pagination" -w codex-claude-remediate \
  --runner fake-success --step-runner review=fake-review-fix --foreground
```
