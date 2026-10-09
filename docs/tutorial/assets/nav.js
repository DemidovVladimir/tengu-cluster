/* The page list of the Tengu field manual: one entry per feature page.
   Order = reading order (drives the rail, the index cards and prev / next).
   A new page = one entry here + one [pages.<slug>] block in ../sources.toml
   (tests/tutorial_map.rs checks the two agree and every page file exists). */
window.TUTORIAL = {
  chapters: [
    { id: "start", kanji: "始", name: "Start here", blurb: "What Tengu is and how one message travels through it." },
    { id: "core", kanji: "核", name: "Core loop", blurb: "Planner picks, subagents work, engines think, tools act." },
    { id: "team", kanji: "組", name: "Build a team", blurb: "Agents, tools, skills and chat surfaces, all composed in TOML." },
    { id: "memory", kanji: "脳", name: "Memory", blurb: "Open Brain for live memory, the LLM Wiki for reviewed knowledge." },
    { id: "safety", kanji: "守", name: "Safety", blurb: "Scopes, Tor egress and secrets: the fences around every tool call." },
    { id: "runtime", kanji: "動", name: "Runtime", blurb: "Long-running loops, typed observations, the numbers and traces they leave, and the Studio that shows them." },
    { id: "desk", kanji: "市", name: "Trading desk", blurb: "Solana LP tools and the paper desk with its risk gate." },
    { id: "research", kanji: "史", name: "Research", blurb: "History first: backfill, backtest, grade forward runs, rank strategies, cite approved sources." },
    { id: "opportunity", kanji: "機", name: "Opportunities", blurb: "Software opportunities: sourced evidence, exact money, HOLD as a real answer." },
    { id: "operate", kanji: "営", name: "Operate", blurb: "Diagnose, deploy and clean up." }
  ],
  pages: [
    { slug: "big-picture", chapter: "start", title: "One message, end to end", blurb: "A prompt goes in, a plan comes out, subagents work, a reply comes back." },

    { slug: "planner", chapter: "core", title: "Planner", blurb: "The LLM whose only job is to emit plan JSON." },
    { slug: "subagents", chapter: "core", title: "Subagent steps", blurb: "Each plan step runs as its own child process with its own tools." },
    { slug: "tool-loop", chapter: "core", title: "Chat turn and tool loop", blurb: "How an agent thinks, calls tools and stays inside its context window." },
    { slug: "engines", chapter: "core", title: "Engines", blurb: "OpenRouter, local models and Claude Code behind one port." },
    { slug: "mcp-bridge", chapter: "core", title: "MCP bridge", blurb: "How Claude Code reaches Tengu's tools with the same rules." },

    { slug: "sandboxes", chapter: "team", title: "Sandbox config", blurb: "One TOML file defines the whole team." },
    { slug: "tools", chapter: "team", title: "Tool catalog", blurb: "One catalog row per tool family; defaults on, extras opted in." },
    { slug: "skills", chapter: "team", title: "Skills", blurb: "Markdown know-how and shell tools, found in three tiers." },
    { slug: "skill-lifecycle", chapter: "team", title: "Skill evals and evolution", blurb: "Score a skill, rewrite it, keep the better version." },
    { slug: "channels", chapter: "team", title: "Chat surfaces", blurb: "Terminal UI, Telegram and webhooks feed the same runtime." },

    { slug: "memory", chapter: "memory", title: "Open Brain and LLM Wiki", blurb: "Postgres memory with recall lanes, compiled into a reviewed wiki." },
    { slug: "local-memory", chapter: "memory", title: "Workspace memory", blurb: "Profile files, vector recall and small stores per workspace." },

    { slug: "scopes", chapter: "safety", title: "Scopes and hardening", blurb: "Every tool call checks what it may touch, deny by default." },
    { slug: "egress", chapter: "safety", title: "Tor egress", blurb: "All traffic leaves through one policy, Tor unless a sandbox opts out." },
    { slug: "secrets", chapter: "safety", title: "Secrets and redaction", blurb: "An encrypted vault in, redacted text out." },
    { slug: "sealed-keys", chapter: "safety", title: "Sealed keys", blurb: "No provider key on this machine: keys are Cloudflare Worker secrets; a route-scoped session names a route, the Worker puts the key at its one spot and forwards." },

    { slug: "decision-loop", chapter: "runtime", title: "Decision loop", blurb: "Jev reads the analysis map, walks the execution map; tools do the work." },
    { slug: "observations", chapter: "runtime", title: "Typed observations", blurb: "Tool results with status, age and features, cached and recorded." },
    { slug: "runtime", chapter: "runtime", title: "tengu run", blurb: "One long-running process per sandbox: feeds, loops, leases, heartbeat." },
    { slug: "metrics", chapter: "runtime", title: "Metrics and audit trails", blurb: "Every model call leaves a record; every risky action leaves a line." },
    { slug: "trace", chapter: "runtime", title: "Execution trace", blurb: "One ordered, redacted event file per run; every event names its cause; read back with tengu trace or in Studio." },
    { slug: "studio", chapter: "runtime", title: "Studio graph and trace", blurb: "A sandbox as a graph from its config; every run as an ordered, redacted event file; a local browser UI." },

    { slug: "solana", chapter: "desk", title: "Solana LP tools", blurb: "Read pools and positions; simulate or send writes behind a lease." },
    { slug: "paper-desk", chapter: "desk", title: "Paper desk and risk gate", blurb: "Every order passes the risk gate inside the tool, then fills on paper." },

    { slug: "history", chapter: "research", title: "Market history", blurb: "Backfill public history into one warehouse before asking anything." },
    { slug: "backtest", chapter: "research", title: "Backtests", blurb: "A pure engine that proves no future bar can change a past decision." },
    { slug: "evidence", chapter: "research", title: "Forward evidence", blurb: "Freeze what a forward run recorded, then grade it from the books it saw." },
    { slug: "lineage", chapter: "research", title: "Lineage and generations", blurb: "Every idea, run and verdict as a record; a frozen generation cannot drift." },
    { slug: "strategy-ranking", chapter: "research", title: "Strategy ranking", blurb: "A sealed contract says how strategies rank, weakest first, on the same data." },
    { slug: "source-evidence", chapter: "research", title: "Source evidence", blurb: "Facts from approved sources with full provenance; an inference is never a fact." },

    { slug: "soe", chapter: "opportunity", title: "Software opportunities", blurb: "tengu soe: a weekly cycle gates and ranks software opportunities on a private, signed profile; agents only propose and challenge." },

    { slug: "ops", chapter: "operate", title: "Diagnose and deploy", blurb: "doctor, prune, Docker and the Tor proxy." }
  ]
};
