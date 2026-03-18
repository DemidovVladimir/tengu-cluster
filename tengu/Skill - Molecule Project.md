---
tags:
  - skill
  - desci
  - molecule
aliases:
  - molecule-project
---

# Skill: Molecule Project

Create a Molecule project (data room) linked to a minted IP-NFT.

| Field | Value |
|-------|-------|
| Skill file | `skills/molecule-project/SKILL.md` |
| Type | Documentation (frontmatter) |
| Base URL | `$MOLECULE_LABS_URL` |
| Auth | x-api-key + x-service-token |

## Tools Used

- `http_request` — GraphQL `CreateProject` mutation

## Input

- `ipnft_symbol` and `ipnft_token_id` from [[Skill - IP-NFT Mint]] output (`mint/metadata/mint_result.json`)
- Service token from [[Skill - Molecule Auth]] (`uploads/service_token.txt`)

## Output

Saves to `uploads/project_result.json`:
- `ipnft_uid` (format: `{contractAddress}_{tokenId}`)
- `ipnft_symbol`, `ipnft_token_id`
- Project URL

## Environment Variables

| Variable | Usage |
|----------|-------|
| `MOLECULE_LABS_URL` | GraphQL endpoint |
| `MOLECULE_API_KEY` | x-api-key header |

## Relations

- **Requires** [[Skill - Molecule Auth]] (service token) and [[Skill - IP-NFT Mint]] (token data)
- **Feeds into** [[Skill - Molecule Upload]] and [[Skill - Molecule Announcement]] (`ipnft_uid`)
- **Used by agent** `mol_labs` in [[DeSci]] sandbox

## Related

- [[Skill - Molecule Upload]] — next step
- [[Skill - Molecule Announcement]] — final step
