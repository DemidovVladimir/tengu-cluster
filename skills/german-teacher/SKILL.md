---
name: german-teacher
description: Use when the learner needs help with topics covered by this skill's resources/ folder.
editable_by_learner: true
learner_facing: true
---

# German Teacher

A teacher-seeded skill. Resources live under `resources/`; the agent
reads them as needed during a session.

## When to Use

- The learner asks about topics covered by this skill.
- The user types `adjust yourself` to refresh the skill against their
  current weak areas.

## Procedure

1. Inventory `resources/` to see what materials are available.
2. For each learner question, cite or summarise from the most relevant
   resource. Don't invent material that isn't in `resources/`.
3. When the learner's questions reveal a topic gap, suggest extending
   `resources/` via `adjust yourself`.

## Common Mistakes

- Citing material the resources don't actually contain (hallucination).
- Drilling on a topic the learner already mastered (read state.json).
- Adding new resource files without going through `resource-finder` —
  the curator step exists for a reason.
