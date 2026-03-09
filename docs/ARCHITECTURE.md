# Mercury CLI Architecture

This document describes the execution model behind Mercury CLI's v0.2 positioning: Mercury-native CI auto-repair built on a trustworthy local repair core. The emphasis here is product boundary, trust boundaries, and the difference between implemented runtime behavior and roadmap claims.

## Product Boundary

Mercury CLI is not trying to be a generic AI coding shell.

The current wedge is narrower and more defensible:

- input: a local repository plus a failing goal or failing verification command
- planner: Mercury 2 produces bounded repair intent from repository context
- editor: Mercury Edit proposes focused file mutations
- verifier: local commands decide whether a candidate is acceptable
- output: an accepted patch or a rejected run with artifacts that explain why

The intended progression is:

- v0.2: trustworthy local repair core
- v0.3: CI auto-repair alpha on top of that core
- v1.0: true concurrent multi-worktree swarm runtime

## What The Current Runtime Makes True

These are the claims the repo should be able to support today after the runtime refactor:

- repair work happens in disposable workspaces under `.mercury/worktrees/`
- rejected candidates are discarded with the workspace instead of being rolled back in-place
- accepted files are copied back only after local verification succeeds
- accepted writes use an atomic temp-file-and-rename path rather than partial file overwrites
- runs emit evidence under `.mercury/runs/` so decisions are inspectable later

These are the claims the docs should not overstate yet:

- measured swarm speedup from `--max-agents`
- broad language support beyond the Rust-first path
- fully autonomous CI draft PR creation
- blanket end-to-end strict schema enforcement across every planner callsite

## Runtime Layers

### 1. Repository context and targeting

The CLI gathers repository context, language information, failure signals, and stored thermal state before asking the planner for a repair strategy.

Responsibilities:

- identify relevant files and failure regions
- collect context without overstuffing prompts
- preserve enough metadata to explain later why a target was chosen

### 2. Planner and structured outputs

Mercury 2 is responsible for planning and critique, not direct repository mutation.

The product direction for v0.2 is versioned structured planner output. In the current repo, the API layer already supports official-style strict JSON schema requests, and the runtime is being hardened around that contract. Docs should present this as a boundary the product is moving toward, not as permission to treat planner prose as trustworthy.

Planner responsibilities:

- identify likely failing regions
- propose bounded repair steps
- estimate cost and token usage
- emit thermal assessments used for prioritization and reporting

### 3. Edit engine protocol

Mercury Edit is responsible for focused mutation, not repo-wide planning.

Two request shapes matter:

- `Apply Edit`: original code plus concrete update snippets
- `Next Edit`: current file path, current file content with nested `code_to_edit`, optional recently viewed snippets, and chronological unidiff edit history

Protocol compliance matters because the product promise depends on predictable edits rather than ad hoc prompt wrappers.

### 4. Candidate sandbox runtime

Candidate generation and verification happen in an isolated workspace under `.mercury/worktrees/`.

The runtime has three separate states:

- generation: produce a candidate patch inside the workspace
- verification: run parse, test, and lint gates inside the workspace only
- acceptance: copy back accepted results atomically and discard the workspace

This separation is the core trust boundary of v0.2.

### 5. Acceptance, rollback, and crash safety

Mercury CLI should never rely on direct writes to the user worktree as part of failed verification.

Correct behavior:

- rejected candidate: discard workspace only
- accepted candidate: copy verified files back atomically
- crash during verification: user worktree remains unchanged

### 6. Artifacts and evidence

Every repair run should leave behind enough evidence to audit what happened later.

Minimum bundle:

- plan metadata
- candidate diffs
- verifier commands and outputs
- final decision
- timing and cost summary

## Safety Invariants

The v0.2 product promise depends on a short list of non-negotiables:

- a failed run must never dirty the user worktree
- no feature flag should imply concurrency or autonomy that is not actually happening
- model output must be gated by local verification before becoming an accepted change
- every run should emit reproducible evidence

These are product constraints, not optional implementation details.

## Capability Boundaries

### Available in v0.2

- Rust-first repair path
- local CLI workflow
- isolated verification and reproducible artifacts
- Mercury Edit surfaces for apply, complete, and next-edit workflows
- honest preview labels for incomplete surfaces

### Preview in v0.2

- `watch --repair` as an end-to-end autonomous repair loop
- `config set` as a fully dependable config editing surface
- binary install via tagged release until a public release is published

### Roadmap after v0.2

- CI auto-repair alpha
- failure parsers for standard Rust verifier commands
- GitHub Action that produces draft repair evidence
- one bounded critique-and-retry pass on failed verification
- richer artifact bundles and public benchmark reporting

### v1.0 bar

- true parallel candidate execution
- conflict arbitration across overlapping edits
- measurable speedup from higher agent counts
- second language support
- public evidence that the swarm claim is true in runtime behavior, not just config names

## Scheduler Reality Check

Thermal terminology exists in the codebase, but the safe product claim for v0.2 is still a bounded repair runtime, not a proven swarm scheduler.

Be explicit about the distinction:

- v0.2 can use thermal scoring for routing and reporting
- v0.2 should not market `--max-agents` as proof of parallel execution if the executor is still partial or not yet benchmarked
- v1.0 is the right milestone for a true concurrent multi-worktree swarm claim

## Why Thermal State Still Exists

Thermal scores still add value before full swarm execution exists.

They provide:

- prioritization of likely repair zones
- a compact way to summarize complexity and risk
- a reporting surface for plan outputs and status views

That is enough value to keep the concept without overselling the runtime.

## Design Notes

Mercury CLI borrows language from stigmergy, annealing, and swarm systems, but those ideas should remain explanatory until runtime behavior proves them.

A good rule for docs and product messaging:

- describe safety behavior as facts
- describe preview behavior as preview
- describe roadmap behavior as roadmap
- keep theory below the product layer until benchmarks prove it
