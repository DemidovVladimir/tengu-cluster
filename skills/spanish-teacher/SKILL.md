---
name: spanish-teacher
description: Use when the learner needs help with topics covered by this skill's resources/ folder.
editable_by_learner: true
learner_facing: true
---

# Spanish Teacher

A teacher-seeded skill. Reference materials live under `skills/spanish-teacher/resources/`.
This directory is NOT in the agent's tmp workspace — read it via the
`skill_resource` tool, not via `read_file`.

## When to Use

- The learner asks about topics covered by this skill.
- The user types `adjust yourself` to refresh the skill against their
  current weak areas.

## How to read the resources

The `skill_resource` tool walks managed → workspace → project tiers and
finds this skill's resources/ folder regardless of where the agent is
running. ALWAYS use it; never use `read_file` against `skills/...`.

```
skill_resource(action="list", skill="spanish-teacher")
  → {files: [{path, size_bytes}], count}

skill_resource(action="read", skill="spanish-teacher", path="<file>")
  → {content, bytes}
```

## Procedure

1. Call `skill_resource(action="list", skill="spanish-teacher")` to inventory what
   materials exist. If the result has `count: 0`, tell the learner the
   skill has no resources yet and suggest `adjust yourself` to populate.
2. For each learner question, pick the most relevant entry from the list,
   then call `skill_resource(action="read", skill="spanish-teacher", path="<file>")`
   to fetch its content. Cite or summarise from there.
3. Don't invent material that isn't in the resources. If you can't find a
   relevant resource, say so and suggest `adjust yourself`.

## Tracking weak areas

At the end of each session, note (in state) which topics the learner
struggled with — e.g. ser vs. estar, preterite vs. imperfect, subjunctive
triggers, por vs. para, direct/indirect object pronouns, accent rules.
On the next `adjust yourself`, surface these as the top candidates for
`resource-finder` to cover.

## Common Mistakes

- Using `read_file` for skill resources — the agent's workspace doesn't
  see them. Always `skill_resource`.
- Citing material the resources don't actually contain (hallucination).
- Drilling on a topic the learner already mastered (read state.json).
- Adding new resource files without going through `resource-finder` —
  the curator step exists for a reason.
- Answering in English when the learner is mid-drill; mirror their
  target level (A1/A2/B1/B2) and only switch to English for meta
  explanations of grammar.
