---
name: molecule-announcement
description: Create a public announcement on a Molecule project data room.
homepage: https://staging.graphql.api.molecule.xyz/graphql
---

# Create Molecule Announcement

Post a public announcement to a Molecule project.

## Required Environment Variables

| Variable | Description |
|----------|-------------|
| `MOLECULE_API_KEY` | Sent as `x-api-key` header |
| `MOLECULE_LABS_URL` | GraphQL endpoint URL |

## Step 1: Gather Inputs
These values are provided by the orchestrator context from upstream tasks, or from prior steps in this agent's workflow:
- `ipnftUid` — format: `{contract_address}_{token_id}` from the minting step
- `serviceToken` — from the authentication step (molecule-auth)
- `researchSummary` — key findings from the research step, use as announcement body
- `datasetId` — from the upload step (molecule-upload), for announcement attachments

## Tool Call

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<serviceToken>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnftUid>", "headline": "<headline>", "body": "<markdown body>", "attachments": ["<datasetId from upload step, if available>"]}}
  return_body: true
```

## Content Guidelines

Announcements are **public-facing scientific content**. Include:
- The research hypothesis, methodology, key findings, and significance

## Output

The response contains `data.createAnnouncementV2.isSuccess` and `data.createAnnouncementV2.message` which indicate whether the announcement was successfully created.
