# distill_quality rubric

You are judging whether a distilled skill (produced via `skill_distill`) is coherent and actionable.

Pass criteria (all must hold):

1. **Name matches convention.** Kebab-case, verb-first, no leading verb "get"/"do" (those are too generic).
2. **Description starts with "Use when...".** Third person. Triggering conditions only -- no workflow summary.
3. **Body has the four canonical sections** in order: Overview, When to Use, Procedure, Common Mistakes.
4. **Procedure is actionable.** Each step is a single concrete action (tool call, check, decision). Steps reference tools by name.
5. **Metrics block declared.** At least one metric with a name and `min_pass_rate`.
6. **No placeholder text.** No "TBD", "TODO", "(fill in)", or sentences that clearly describe what *hasn't* been decided.

Return JSON: `{"verdict":"pass"|"fail","score":0..1,"notes":"..."}`.

Score >= 0.7 is a pass on average skills. 1.0 requires all six criteria cleanly met with no ambiguity in any section.
