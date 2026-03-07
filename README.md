# Mercury CLI 🌡️

**The first diffusion-native CLI for autonomous code synthesis.**

Mercury CLI doesn't treat diffusion models as fast, cheap transformers. It treats them as a fundamentally different cognitive architecture — and builds native tooling to match.

---

## The Problem

Every AI coding tool on the market — Claude Code, GitHub Copilot, Cursor, Codex, OpenClaw — is built on transformer assumptions. Sequential planning. Linear pipelines. One model holds context, reasons step-by-step, generates token-by-token.

That made sense when every model was autoregressive.

**It doesn't make sense for diffusion.**

[Inception Labs' Mercury 2](https://www.inceptionlabs.ai) is a diffusion-based large language model. It doesn't predict the next token. It resolves the entire output field in parallel through iterative refinement — the same way image diffusion models work, applied to text and code. This isn't a faster transformer. It's a different topology of intelligence.

Mercury CLI is built for that topology.

## The Insight

A diffusion model's small context window isn't a limitation. **It's a design principle.**

Instead of one model holding your entire codebase and reasoning sequentially, Mercury CLI spawns many focused agents — each with a tight receptive field scoped to a single file, function, or pattern. The agents don't talk to each other. They read and write to a **shared thermal field**.

This is [stigmergy](https://en.wikipedia.org/wiki/Stigmergy) — the same coordination mechanism ants use. No central planner. No message passing. Just a shared environment that agents modify, and those modifications guide subsequent agents. O(N) scaling instead of O(N²).

## How It Works

### Heat Maps, Not Task Lists

Mercury CLI doesn't plan sequentially. It generates a **thermal heat map** of your codebase:

```
src/
├── main.rs                    ░░░░░░░░░░  0.12  LOCKED
├── engine/
│   ├── planner.rs             ▓▓▓▓▓░░░░░  0.54  [1 agent]
│   ├── scheduler.rs           ▓▓▓▓▓▓▓▓▓░  0.91  [4 agents]
│   └── verifier.rs            ░░░░░░░░░░  0.15  LOCKED
└── swarm/
    ├── agent.rs               ▓▓▓▓▓▓░░░░  0.63  [1 agent]
    └── spawner.rs             ▓▓▓▓░░░░░░  0.41

Global Temperature: 0.78 | Iteration: 3/10 | Budget: $0.23/$0.50
```

**Hotspots** = complexity clusters, dense dependencies, high risk.
**Cool zones** = stable, well-understood code.

Execution follows the thermal gradient:

1. **Cool zones first** — resolve simple code, verify it, lock it as immutable scaffolding
2. **Hot zones last** — attack complexity with multiple agents and iteration passes
3. **Anneal** — progressively freeze the system toward a stable state

This isn't arbitrary. It's [Simulated Reverse Annealing](https://en.wikipedia.org/wiki/Simulated_annealing) — a proven optimization strategy that outperforms forward search by starting from verified boundary conditions instead of maximum entropy.

### The Swarm

At low concurrency, Mercury CLI is a normal dev tool. At high concurrency, it becomes a **swarm**.

```bash
# 20 agents (default) — focused development tool
mercury fix "refactor auth to use exponential backoff"

# 200 agents — full swarm mode
mercury fix "refactor auth to use exponential backoff" --max-agents 200
```

The architecture doesn't change. The **density knob** turns. Same thermal field, same merge engine, same coordination primitive. The swarm emerges from the same code that runs a single agent.

Each agent emits a **micro-heat-map** of its scope after every action. These aggregate upward into the project-level thermal model via [Log-Sum-Exp](https://en.wikipedia.org/wiki/LogSumExp) merging — a smooth, differentiable aggregation that preserves gradient topology without saturating.

Monitor agents watch other agents. They detect conflicting patches, semantic drift, and oscillation patterns. When two agents enter a limit cycle of competing optimizations, the monitor triggers a temperature reduction that forces convergence.

### Pheromone Decay

Biological pheromones evaporate. Digital ones must too.

Every thermal score in the database decays exponentially over a configurable half-life. This prevents the swarm from fixating on stale hotspots and enables continuous adaptation to a changing codebase. Without decay, the system traps itself in positive feedback loops. With decay, it stays plastic.

## Architecture

```
┌─────────────────────────────────────────┐
│  Planner/Router    (Mercury 2 - 128K)   │  → Repo-level planning, heat map generation
├─────────────────────────────────────────┤
│  Patch Engine      (Mercury Edit - 32K) │  → Focused edits with auto-tagged payloads
├─────────────────────────────────────────┤
│  Verifier          (Local-first)        │  → Parse, test, lint BEFORE any write
├─────────────────────────────────────────┤
│  Scheduler         (Thermal Merge)      │  → Concurrency, decay, budget, aggregation
└─────────────────────────────────────────┘
```

**Mercury 2** sees the big picture (128K context) — planning, routing, critique.
**Mercury Edit** does precise surgery (32K context) — only the exact slice the router hands it.
**Local tools** verify first, always — parse, test, lint run before any model touches your code.
**The Scheduler** is the nervous system — thermal merge, pheromone decay, swarm density, budget.

## Commands

```bash
# Initialize Mercury in your repo
mercury init

# Generate thermal heat map
mercury repo plan "fix flaky auth tests"

# View live thermal state
mercury status --heatmap

# Ask Mercury 2 about your codebase
mercury ask "why does the auth module have so many dependencies?"

# Apply a targeted edit
mercury edit apply src/auth.rs --instruction "convert retries to exponential backoff"

# The killer workflow: plan → index → patch → verify → commit
mercury fix "refactor auth module" --max-agents 50 --max-cost 1.00

# Watch tests and auto-repair failures
mercury watch "cargo test" --repair

# Run a custom workflow
mercury agent run .mercury/repair.yml
```

## Theoretical Foundations

This isn't just engineering. The architecture is validated by four independent theoretical frameworks:

- **Stigmergy** (Grassé, 1959) — Indirect coordination via environmental modification. The thermal field is a digital pheromone map. Agents coordinate at O(N) instead of O(N²).

- **Simulated Reverse Annealing** — Cool-to-hot execution creates verified boundary conditions before attacking complexity. Proven to outperform forward annealing in combinatorial optimization.

- **Mean-Field Games** (Lasry & Lions, 2006) — The thermal field functions as a Fokker-Planck density distribution. Agents optimize against the macroscopic swarm density, not individual peers. Dynamic load balancing without centralized scheduling.

- **Active Inference / Free Energy Principle** (Friston) — Agents minimize Expected Free Energy along the thermal gradient. Cool zones = pragmatic value (exploit). Hot zones = epistemic value (explore). The swarm naturally balances exploitation and exploration.

For the full theoretical analysis, see [ARCHITECTURE.md](docs/ARCHITECTURE.md).

## The Budget Model

Mercury CLI isn't about cheap inference. It's about a **different topology of intelligence**.

For the same budget as one frontier model call, you get 200 focused agents operating in parallel. The intelligence isn't in any single agent. It emerges from the swarm — distributed, parallel, convergent.

| Approach | Agents | Intelligence | Scaling |
|----------|--------|-------------|---------|
| One Opus call | 1 | Deep sequential reasoning | O(1) |
| Mercury CLI swarm | 200 | Distributed emergent intelligence | O(N) linear |

## Installation

```bash
# From source
git clone https://github.com/denster32/mercury-cli
cd mercury-cli
cargo build --release

# Set your API key
export MERCURY_API_KEY="your-inception-api-key"

# Initialize in your repo
cd your-project
mercury init
```

## Configuration

```toml
# .mercury/config.toml

[scheduler]
max_concurrency = 20         # swarm density (1-500)
max_cost_per_command = 0.50  # USD budget cap

[thermal]
decay_half_life_seconds = 300   # pheromone evaporation rate
hot_threshold = 0.7             # complexity threshold
cool_threshold = 0.3            # stability threshold
lock_cool_zones = true          # freeze verified code

[annealing]
enable_global_momentum = true
cooling_rate = 0.02             # convergence speed
```

## Why "Mercury"?

Named for the model it's built around. But also: Mercury the element is liquid metal at room temperature. It flows, fills gaps, finds the lowest point. That's what the thermal gradient does — computational energy flows toward complexity and fills the gaps until the system reaches equilibrium.

## Status

**v0.1** — First release. The paradigm is real. The math works. The swarm is alive.

Built on a Saturday afternoon in Indiana by one developer and five AI models working in parallel — each with a different architecture, none talking to each other, all reading and writing to the same shared field.

Stigmergy scales.

## Contributing

This is MIT licensed. The value isn't in the code — it's in the paradigm. Fork it, extend it, build on it.

If you're from Inception Labs: let's talk about exposing raw diffusion latents in the API. The thermal field would be even more powerful with native uncertainty scores instead of prompted approximations.

If you're from Anthropic: transformers and diffusion aren't competitors. They're complementary architectures. The tooling layer that bridges them is where the real leverage lives.

## License

MIT
