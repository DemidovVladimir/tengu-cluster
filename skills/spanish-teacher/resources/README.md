# Resources

This folder holds the skill's reference material — markdown notes, web links, PDFs, etc. Agents read these via the `skill_resource` tool (not `read_file` — the agent's workspace is a tmp dir and doesn't see this path).

Three ways to populate it:

1. Drop files into this directory directly.
2. Re-run `tengu skill seed <name> <dir>` against a different skill name (this skill is already seeded).
3. In a chat session, type `adjust yourself` — the `resource-finder` agent fetches relevant web sources and the `skill-improver-inline` agent commits them here under an approval gate.
