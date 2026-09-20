---
tags: [idea, research, skills, agents, auto-improvement, karpathy]
status: draft
created: 2026-03-28
reference: https://github.com/karpathy/autoresearch
---

> Idea note, moved from `tengu/ideas/auto-skill-research/Auto-Improving Agent Skills.md` on 2026-09-18.

## Hard Architecture Constraints

These are non-negotiable and must be respected by every design decision in this document:

- **Rust only.** The entire Tengu runtime is Rust. No Python, no TypeScript, no JavaScript, no shell scripts as first-class components. Eval harnesses, scoring functions, orchestration logic — all Rust.
- **Skills are plug-and-play.** A skill is a folder containing a `SKILL.md` file (Markdown). Skills can be added, swapped, or updated at runtime with zero rebuild. This is the single biggest architectural advantage for an evolution loop — the researcher can write a new skill version and it is immediately live.
- **Tools require a rebuild.** Tools are Rust structs implementing `ToolExecutionPort`, compiled into the Tengu binary. Adding or modifying a tool means `cargo build`. This means the evolution loop can freely mutate skill prompts (hot-swap), but cannot add new tool capabilities without a deliberate rebuild cycle.

---

# Auto-Improving Agent Skills in Tengu

## The Inspiration: Karpathy's autoresearch

Andrej Karpathy's [`autoresearch`](https://github.com/karpathy/autoresearch) project is a beautifully simple loop:

1. An AI agent edits `train.py` (the only file it can touch)
2. It runs a fixed 5-minute training experiment
3. It measures the outcome — one metric: validation loss (bits per byte)
4. It keeps or discards the change
5. Repeat overnight → ~12 experiments/hour, unattended

The key insight is the **constraint**: one editable file, one metric, one fixed time budget. That constraint is what makes the loop runnable without human oversight.

The human's job shifts from *doing research* to *writing `program.md`* — a Markdown file that gives the agent its research direction and context. The agent does the rest.

---

## The Problem in Tengu

Tengu's skills (the instruction sets that shape agent behavior — SKILL.md files, prompt templates, tool descriptions) are currently **static**. They're written once and never updated unless a human manually revises them.

But skills decay. A skill that worked well in February may underperform in April as models improve, user workflows evolve, or new tools become available. There's no feedback loop.

**The question:** can we build an autoresearch-style loop that automatically improves Tengu's skills over time, without human intervention?

---

## The Idea: Skill Evolution Loop

Map autoresearch's architecture onto Tengu's skill system:

| autoresearch component | Tengu equivalent |
|---|---|
| `train.py` (the editable file) | `SKILL.md` (the skill prompt/instructions) |
| `program.md` (human guidance) | A `research_goal.md` per skill folder — what "better" means |
| Training run (5 min) | A **benchmark task** — a fixed eval set for that skill |
| Validation loss | A **skill quality score** (see below) |
| Agent that edits code | A **Skill Researcher agent** in Tengu |
| Overnight loop | A scheduled Tengu orchestration run |

### The Loop

```
┌─────────────────────────────────────────┐
│  Load current SKILL.md + research_goal  │
│                                         │
│  Skill Researcher agent proposes edits  │
│  (guided by research_goal.md)           │
│                ↓                        │
│  Run benchmark tasks with new skill     │
│                ↓                        │
│  Score outputs against eval criteria    │
│                ↓                        │
│  Score improved? → keep edit + log      │
│  Score worse?   → discard + log reason  │
│                ↓                        │
│  Repeat (budget: N experiments/night)   │
└─────────────────────────────────────────┘
```

---

## The Hard Part: Defining the Metric

In autoresearch, the metric is trivial — validation loss is a single float. For skill quality, the metric is harder. A few approaches:

### Option A: LLM-as-Judge
After each benchmark run, a separate judge agent scores the output against a rubric defined in `research_goal.md`. Flexible, but adds cost and introduces noise. Good for subjective skills (writing, summarization).

### Option B: Functional eval
For skills with deterministic outputs (code generation, data extraction, file parsing), run actual assertions. Did the generated DOCX have a table of contents? Did the XLSX have the right formulas? Pass/fail scoring. Most reliable but requires writing evals upfront.

### Option C: User feedback signals
Track thumbs-up/thumbs-down from real usage and use accumulated signal as a lagging quality metric. Zero cost per experiment, but requires enough usage volume to be meaningful. Best as a long-term signal layered on top of A or B.

**Recommended starting point:** Option B for skills where it's feasible, Option A as fallback. Option C as a long-term improvement signal.

---

## Skill Researcher Agent Design

The Skill Researcher is a Tengu agent (orchestrated via `orchestrate` command) with:

- **Read access** to the current `SKILL.md` and `research_goal.md`
- **Write access** to a `SKILL.candidate.md` (never overwrites `SKILL.md` directly — only the eval harness does that after scoring)
- **A fixed research budget**: e.g. 10 experiments per run, each capped at 60 seconds of eval time
- **A changelog**: every accepted edit is appended to `SKILL.changelog.md` with score delta, date, and a one-sentence reason

The agent is intentionally narrow. It cannot browse the internet, modify other files, or call external APIs. It just reads, proposes, and the harness judges.

---

## research_goal.md: The Human Interface

Just like `program.md` in autoresearch, each skill folder gets a `research_goal.md` written by a human. This is the single point of human control. Example for a `summarize` skill:

```markdown
## Goal
Improve the summarize skill so that the agent:
- Produces summaries that are always under 150 words
- Preserves key named entities (people, projects, dates) without hallucination
- Structures output as: one-sentence TL;DR → key points → open questions

## What NOT to change
- The overall SKILL.md section structure (keep headings, refine content only)
- Tool usage — the skill must not require any tools that aren't already compiled in

## Benchmark tasks
See /evals/summarize/ — 5 input documents with reference outputs scored by the judge agent
```

> **Note:** `research_goal.md` must never suggest adding new tools or new library dependencies. Skill evolution is prompt/instruction evolution only — the compiled Tengu binary is fixed for a given run. If new tool capabilities are genuinely needed, that's a separate engineering task (Rust → rebuild).

---

## Integration with Tengu's Existing Architecture

### Skills are already plug-and-play — exploit this
The most important architectural fact: because skills are just `SKILL.md` files loaded at runtime, the Skill Researcher agent can write `SKILL.candidate.md`, the eval harness swaps it in, runs the benchmark, and swaps it back — all without touching the binary. No rebuild. No restart. This is the core reason this loop is practical in Tengu at all.

### Eval harness is a Rust component, not a script
The harness that loads skills, fires benchmark tasks, and scores outputs must be implemented in Rust as a new `EvalHarness` struct inside `adapters/`. It gets wired into a dedicated `tengu eval` CLI subcommand. No Python eval scripts, no shell wrappers — the harness is a first-class compiled part of Tengu.

```rust
// Rough shape — adapters/eval_harness.rs
pub struct EvalHarness {
    skill_path: PathBuf,       // path to SKILL.md under evaluation
    tasks: Vec<EvalTask>,      // loaded from /evals/<skill-name>/
    judge: Arc<dyn EvalJudge>, // LLM-as-judge or functional scorer
}

impl EvalHarness {
    pub async fn run(&self, candidate: &str) -> Result<EvalScore> { ... }
}
```

Adding `EvalHarness` is a **one-time rebuild**. After that, all skill evolution happens through hot-swappable Markdown — no further rebuilds needed.

### Scheduled orchestration
Use Tengu's `orchestrate` command + a dedicated sandbox (`sandboxes/skill-research/`) to run the evolution loop on a schedule (e.g. every night at 2am). The sandbox config points the orchestrator to the Skill Researcher agent profile.

### Skill version control
Before any edit, copy `SKILL.md` → `SKILL.v{N}.md` in a `/versions/` subfolder. If a future experiment degrades quality, rollback is one file copy. Git history also provides this, but explicit versioning makes it visible inside Obsidian.

### Memory integration
The Skill Researcher agent should have access to Tengu's permanent memory store (see [[Permanent Memory in Tengu]]) so it can accumulate cross-session knowledge: "we tried adding examples in March, it helped; we tried shortening the system prompt in February, it hurt." Since memory is already a compiled Rust subsystem, no rebuild is needed to enable this.

### Progressive rollout
Don't replace `SKILL.md` immediately. Introduce an A/B mechanism: 20% of agent runs use `SKILL.candidate.md`, 80% use `SKILL.md`. Because skill loading is dynamic, the A/B split requires no binary changes — just a config flag in the sandbox `config.toml`. Promote candidate to production only after consistent improvement across real tasks, not just benchmarks.

---

## What Gets Better Over Time

With this loop running, skills can improve along several axes:

**Instruction clarity** — the researcher discovers which phrasings produce more reliable outputs and refines the language.

**Example quality** — few-shot examples in skill prompts can be swapped for better ones as better examples are generated and scored.

**Edge case handling** — the eval suite catches failure modes; the researcher learns to pre-empt them.

**Model-specific tuning** — as Tengu's underlying model is upgraded, skill prompts that relied on old model behaviors can be automatically re-tuned.

---

## Open Questions

- **Eval suite maintenance**: who writes the benchmark tasks? This is the main ongoing human cost. Could a second agent propose new eval cases based on observed failures?
- **Cost control**: LLM-as-judge calls can add up overnight. Need hard budget caps per skill per night.
- **Skill interdependencies**: some skills reference others (e.g. `docx` might call into `pdf` skill patterns). Improving one in isolation might break the other. Need a cross-skill regression check.
- **Metric gaming**: the researcher could overfit to the specific benchmark tasks. Periodic refresh of the eval suite (by a human or another agent) is important.
- **When to stop**: should there be a "good enough" threshold where a skill exits the active evolution pool and only re-enters if degradation is detected?

---

## Next Steps

- [ ] Implement `EvalHarness` struct in `adapters/eval_harness.rs` (Rust) — this is the one required rebuild
- [ ] Add `tengu eval <skill-name>` CLI subcommand wired to `EvalHarness`
- [ ] Write `research_goal.md` for one pilot skill (pick one with deterministic, assertable outputs)
- [ ] Create `/evals/<skill-name>/` with 5 benchmark tasks as `.toml` or `.md` input/output pairs
- [ ] Implement `Skill Researcher` agent profile in `sandboxes/skill-research/config.toml`
- [ ] Add skill versioning convention (`/versions/` subfolder) to existing skill folders
- [ ] Wire into Tengu's scheduled orchestration
- [ ] After first 2-week run: review `SKILL.changelog.md`, assess whether accepted edits represent real improvements
- [ ] Consider a "skill health dashboard" note auto-generated by Tengu → Obsidian, showing score history per skill

---

## References

- [karpathy/autoresearch](https://github.com/karpathy/autoresearch) — the original loop: one file, one metric, unattended overnight runs
- [[Permanent Memory in Tengu]] — memory layer the Skill Researcher can use to accumulate cross-session learning
