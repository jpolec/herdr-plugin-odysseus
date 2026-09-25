# Vision: contract-locked delivery

Status: proposal. Nothing here is implemented yet; everything it needs
already exists in 0.2.0 (policy engine, diff gate, acceptance criteria,
epics, variants, audit chain).

## The bottleneck is no longer writing code

With epics, dependencies, retries and reviewers, the orchestrator can
produce more pull requests in a day than a person can responsibly read.
Agents are cheap and parallel; **human review is serial**. Every further
improvement in *generation* makes the queue of unreviewed PRs longer. A
10× feature therefore has to cut the cost of *trusting* a change, not of
producing it.

Today, trusting an agent's PR means reading the implementation: hundreds
of lines, in an unfamiliar style, looking for the one wrong branch. The
acceptance review helps, but it is another agent's opinion — and the
same agent that is judged also writes the tests that judge it. An agent
under pressure to make checks pass will, sooner or later, make the
*checks* easier (we already guard against the crude forms of this:
`approve-disabled-tests`, `guard-test-only-retry`).

## The idea

Split each task into two artifacts with different owners, and lock the
one the human approved:

```text
acceptance criteria ──► CONTRACT agent writes executable tests
                            │   orchestrator proves they FAIL on the base
                            ▼   (a contract that already passes proves nothing)
                    human reviews the contract (small, domain-level diff)
                            │   approval pins the contract: path set + content hash
                            ▼
      IMPLEMENTER agent(s) work with the contract LOCKED by policy
      (any write/delete to a contract file = DENY, in real time for Claude
       via the PreToolUse hook, at the diff gate for everyone)
                            │   checks: contract green, full suite green
                            ▼
      PR with a verifiable receipt: "contract <hash> approved by <human>
      at <time>; unchanged since; passes on <head>" + audit chain root
```

The human reviews **what** must be true (60 lines of tests that read like
the acceptance criteria) instead of **how** it was done (600 lines of
implementation). The orchestrator guarantees — not hopes — that the
implementation was held to exactly that contract.

## Why this is 10× and not 2×

1. **Review cost drops by the ratio of implementation size to contract
   size.** Contracts are typically 5–20× smaller than the code that
   satisfies them, and they are written in the vocabulary of the task, not
   of the codebase. A reviewer can approve a contract for a module they
   have never read.
2. **Tournaments become objective.** Today `--variants 3` ends with a human
   comparing three diffs. Against a locked contract every variant is
   measured by the same executable standard; the orchestrator can rank
   them by facts (contract green, suite green, diff size, new
   dependencies, policy asks, tokens) and the human picks among
   *equivalent* solutions. Running five cheap implementers against one
   contract becomes a sound strategy instead of a review burden.
3. **Agent PRs become safe to merge by rule, not by faith.** A team can
   state "a contract-locked PR whose contract I approved, that touches
   only its scope and raised no policy asks, needs no line-by-line review"
   — and the receipt proves every clause. That is the step from assisted
   coding to delegated delivery.
4. **It compounds with epics.** An ADR becomes a set of contracts. The
   conformance review stops being an opinion ("covered?") and becomes a
   query: which Decision statements have an approved, green contract.
5. **It is hard to copy.** It needs, at once: a policy engine that can
   lock paths per run, a pre-execution hook, a diff gate, human approvals
   with context, variants, and a tamper-evident audit trail. Herdr runs the
   agents; this plugin is the only place where all of that meets.

## Design sketch (on top of 0.2.0)

- **`type: contract` step** (`output: contract`): the agent writes tests
  for `{{acceptance}}` and reports `{"files": [...], "criteria_map":
  {"1": ["test_name", ...]}}`. The orchestrator then:
  - runs the named check (or the plan's verification commands) on the
    contract commit and **requires it to fail** ("red" proof), and
    requires every criterion to map to at least one test;
  - commits the contract separately and asks for approval with the
    contract diff, the criteria map and the red output.
- **Run-scoped policy.** On approval the driver adds an in-memory rule
  `deny write/delete <contract files>` with source `contract:<approval
  id>` and records `contract_sha256` (hash over the files' content) in the
  run. The rule is evaluated by the diff gate and served to the Claude
  hook (`hook claude-pretool` already loads the run; it would add the
  run's contract rules). No policy file is ever edited.
- **Green proof.** Before the PR step: contract files' hash unchanged,
  contract check green, full suite green. Any mismatch blocks the run.
- **Receipt.** The PR body gets a machine-readable block (acceptance
  table — already there in 0.2.0 — plus `contract_sha256`, approver,
  approval time, audit chain head). `herdr-orchestrator receipt verify
  <pr-url>` re-computes the hash on the PR head and checks the local
  audit chain. Signing the audit head (ROADMAP §3.10) makes receipts
  verifiable by other people.
- **Tournament ranking.** `task compare` gains a "contract" column and
  sorts equivalent variants by diff size and policy asks; it still never
  picks a winner by itself.
- **Escape hatch.** A contract can be wrong. The implementer may *propose*
  a contract change (written to `.herdr-orchestrator/out/`, never applied);
  the human approves a new contract version, which re-pins the hash. The
  history of contract versions is part of the receipt.

## Phases

1. `output: contract` + red proof + approval + per-run lock at the diff
   gate. Workflow `contract-first`: contract → approve → implement → green
   proof → acceptance review → PR.
2. Lock served to the Claude hook; receipt block in the PR body;
   `receipt verify`.
3. Variants against one contract with ranking; epics that plan contracts
   per task; conformance review as a contract query.
4. Signed audit heads; team-level "merge by rule" policies.

## Risks

- **Weak contracts.** Tests that pass trivially after implementation.
  Mitigations: the red proof on base; the criteria map; optional mutation
  check of the contract against the implementation (a contract that
  survives deleting the implementation's key lines is too weak).
- **Over-specified contracts** that dictate implementation details and
  make agents fight the tests. The contract skill tells the agent to test
  behaviour through public interfaces; the human sees exactly this at
  approval time, when it is cheapest to fix.
- **Not everything is testable** (UX, performance under real load). Those
  criteria stay `unverifiable`/manual, as today — the receipt says so
  honestly instead of pretending.

## Runners-up (and why they are not the 10×)

- *Policies learned from approvals* (repeated "yes" to the same rule and
  path becomes a proposed, scoped, expiring allow rule): real friction
  reduction, but it makes the *existing* loop faster; it does not change
  what a human has to read. Worth doing after contracts, because contracts
  make approvals rarer and more meaningful.
- *Policy broker for every agent in Herdr* (not only the plugin's own):
  high leverage, but it depends on upstream Herdr hooks
  (UPSTREAM_REQUESTS §4) and protects rather than accelerates.
