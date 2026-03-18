---
tags:
  - skill
  - desci
  - molecule
aliases:
  - molecule-announcement
---

# Skill: Molecule Announcement

Create a public announcement on a Molecule project data room.

| Field | Value |
|-------|-------|
| Skill file | `skills/molecule-announcement/SKILL.md` |
| Type | Documentation (frontmatter) |
| Base URL | `$MOLECULE_LABS_URL` |
| Auth | x-api-key + x-service-token |

## Tools Used

- `http_request` — GraphQL `CreateAnnouncement` mutation

## Input

- `ipnft_uid` from [[Skill - Molecule Project]]
- Service token from [[Skill - Molecule Auth]]
- `headline` and `body` (markdown)

> [!warning] Content guidelines
> Include hypothesis, methodology, key findings, significance. Do NOT include internal data: merkle roots, tx hashes, metadata CIDs, reservation IDs, token IDs, wallet addresses.

## Output

Saves to `uploads/announcement_result.json`:
- `id`

## Environment Variables

| Variable | Usage |
|----------|-------|
| `MOLECULE_LABS_URL` | GraphQL endpoint |
| `MOLECULE_API_KEY` | x-api-key header |

## Relations

- **Requires** [[Skill - Molecule Auth]] and [[Skill - Molecule Project]]
- **Last step** in the Molecule data room phase of [[DeSci]]
- **Used by agent** `mol_labs` in [[DeSci]] sandbox
