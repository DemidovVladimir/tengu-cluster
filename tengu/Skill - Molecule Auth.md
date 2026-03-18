---
tags:
  - skill
  - desci
  - molecule
aliases:
  - molecule-auth
---

# Skill: Molecule Auth

4-step authentication flow to obtain a Molecule service token for project and upload operations.

| Field | Value |
|-------|-------|
| Skill file | `skills/molecule-auth/SKILL.md` |
| Type | Documentation (frontmatter) |
| Base URL | `$MOLECULE_LABS_URL` |
| Auth | x-api-key (`MOLECULE_API_KEY`) |

## Tools Used

- `get_wallet_address` — resolve wallet for sign-in
- `http_request` — GraphQL calls (get message, exchange token)
- `sign_message` — sign the authentication message

## 4-Step Workflow

1. `get_wallet_address` — get the Privy wallet address
2. `http_request` — GraphQL query to get sign-in message (service name: `"tengu-agent"`)
3. `sign_message` — sign the message via Privy
4. `http_request` — GraphQL mutation to exchange signature for service token (expiry: `"720h"`)

## Output

Saves service token to `uploads/service_token.txt`. Used as `x-service-token` header in subsequent Molecule calls.

## Environment Variables

| Variable | Usage |
|----------|-------|
| `MOLECULE_LABS_URL` | GraphQL endpoint |
| `MOLECULE_API_KEY` | x-api-key header |

## Relations

- **Required before** [[Skill - Molecule Project]], [[Skill - Molecule Upload]], [[Skill - Molecule Announcement]]
- **Used by agent** `mol_labs` in [[DeSci]] sandbox

## Related

- [[Skill - Molecule Project]] — next step after auth
- [[Wallet]] — Privy wallet used for signing
