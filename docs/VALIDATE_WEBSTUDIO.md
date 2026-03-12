# Webstudio Multi-Agent Validation Guide

## Prerequisites

1. `OPENROUTER_API_KEY` set (via secrets vault or env)
2. `TELEGRAM_BOT_TOKEN` set (via secrets vault or env)
3. Build: `cargo build --features telegram`

The workspace (`~/webstudio-project`) is auto-created by the scaffold config on startup — no manual `mkdir` needed.

## Start

```bash
cargo run --features telegram -- telegram --sandbox webstudio
```

You should see logs showing 5 agents registered:
- frontend (frontend_engineer)
- designer (designer)
- backend (backend_engineer)
- cms (cms_guide)
- marketing (marketing)

## Test 1: List Agents

Send in Telegram:

```
/agents
```

Expected: list of 5 agents with roles and tool counts.

## Test 2: Route to Backend

Send:

```
@backend_engineer: create a file src/server.js with a basic Express hello world
```

Expected: agent asks approval for write_file, creates the file.

## Test 3: Route to Designer

Send:

```
@designer: create docs/design-tokens.md with color palette
```

Expected: designer writes the file (has write_file tool).

## Test 4: Route to CMS (read-only)

Send:

```
@cms_guide: list all files in the project
```

Expected: lists files. CMS agent has only read_file + list_directory.
It should NOT be able to write files or run commands.

## Test 5: Verify Tool Restrictions

Send:

```
@cms_guide: create a file called test.txt with hello
```

Expected: agent cannot write (no write_file tool).

## Test 6: Route to Frontend

Send:

```
@frontend_engineer: read src/server.js and suggest a matching React app
```

Expected: reads the file backend created, gives suggestions.

## Test 7: Sticky Agent

Send a plain message (no @prefix):

```
what files exist in the project?
```

Expected: goes to the LAST agent you talked to.

## Test 8: Marketing

Send:

```
@marketing: write docs/seo-plan.md with SEO strategy for a blog
```

Expected: writes file (has write_file but NOT run_command).

## Test 9: Unknown Role

Send:

```
@devops: set up CI
```

Expected: error message listing available roles.

## Test 10: Other Commands

```
/help
/reset
/cost
```

These apply to the currently active agent.

## What to Look For

- Each agent responds based on its instructions
- Tool restrictions are enforced (cms can't write, designer can't run_command)
- @routing switches between agents
- Each agent keeps separate conversation history
- [AgentName] label shows when routing explicitly

## Orchestrator Mode (Alternative)

```bash
cargo run -- orchestrate --sandbox webstudio
```

Then type:

```
backend_engineer: create a REST API skeleton
frontend_engineer: create a React component
designer: review the color scheme
/fleet
/quit
```
