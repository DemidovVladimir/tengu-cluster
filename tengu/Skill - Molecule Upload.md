---
tags:
  - skill
  - desci
  - molecule
aliases:
  - molecule-upload
---

# Skill: Molecule Upload

Upload a research file to a Molecule project data room via a 3-step S3 presigned URL flow.

| Field | Value |
|-------|-------|
| Skill file | `skills/molecule-upload/SKILL.md` |
| Type | Documentation (frontmatter) |
| Base URL | `$MOLECULE_LABS_URL` (GraphQL) + S3 presigned URL |
| Auth | x-api-key + x-service-token |

## Tools Used

- `http_request` — GraphQL mutations (initiate, finalize) + S3 PUT

## 3-Step Workflow

1. **Initiate** — GraphQL `InitiateUpload` mutation -> returns `presignedUrl` + `uploadId`
2. **Upload** — `http_request` PUT to exact presigned URL (no modifications, content-type: `application/pdf`)
3. **Finalize** — GraphQL `FinalizeUpload` mutation -> returns `datasetId`

> [!important]
> The S3 presigned URL must be used exactly as returned. Do not modify it.

## Input

- `ipnft_uid` from [[Skill - Molecule Project]]
- Service token from [[Skill - Molecule Auth]]
- File path (workspace-relative)

## Output

Saves to `uploads/upload_result.json`:
- `datasetId`

## Environment Variables

| Variable | Usage |
|----------|-------|
| `MOLECULE_LABS_URL` | GraphQL endpoint |
| `MOLECULE_API_KEY` | x-api-key header |

## Relations

- **Requires** [[Skill - Molecule Auth]] and [[Skill - Molecule Project]]
- **Optional for** [[Skill - Molecule Announcement]] (datasets can be attached)
- **Used by agent** `mol_labs` in [[DeSci]] sandbox
