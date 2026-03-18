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

## Input

- `ipnft_uid` from `uploads/project_result.json`
- Service token from `uploads/service_token.txt`
- Research summary for the announcement content
- Optional: `datasetId` from `uploads/upload_result.json` for attachments

## Tool Call

```
http_request:
  url: $MOLECULE_LABS_URL
  method: POST
  headers: {"x-api-key": "$MOLECULE_API_KEY", "x-service-token": "<token>", "Content-Type": "application/json"}
  body: {"query": "mutation CreateAnnouncementV2($ipnftUid: String!, $headline: String!, $body: String!, $attachments: [String!]) { createAnnouncementV2(ipnftUid: $ipnftUid, headline: $headline, body: $body, attachments: $attachments) { isSuccess message error { message code retryable } } }", "variables": {"ipnftUid": "<ipnft_uid>", "headline": "<headline>", "body": "<markdown body>", "attachments": ["<datasetId from upload, if available>"]}}
```

If no file was uploaded or upload failed, omit `attachments` or pass an empty array.

## Content Guidelines

Announcements are **public-facing scientific content**. Include:
- The research hypothesis, methodology, key findings, and significance

Do NOT include internal data:
- Merkle roots, transaction hashes, metadata CIDs
- Reservation IDs, token IDs, wallet addresses

## Output

Save to `uploads/announcement_result.json`.
