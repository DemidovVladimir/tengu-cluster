---
name: telegram-rag-ingest
description: Turn resources shared in Telegram chat (attachments, URLs, pasted text) into searchable vector memory, and answer later questions from it.
homepage: https://github.com/moleculeprotocol/mol-tengu-cluster
---

# Telegram RAG Ingest

This skill teaches you how to persist resources a user shares over Telegram — file attachments, URLs, and pasted text blocks — into a vector index, and how to recall them later by semantic search. Retrieval is embedding-based, not keyword, so always rephrase questions into dense topic phrases before searching.

This skill does **not** call any external HTTP API. It documents the local `persistent_store` tool and supporting platform primitives. Ignore the general "use only the URLs below" preamble — there are no fixed URLs in this skill. When you need to fetch a URL the user shares, use `http_request` with the user's URL directly.

## Precondition

Before using this skill, check once per session that `persistent_store` is in your available tool list. If it is not, tell the user this feature is disabled and point them at the config snippet at the bottom of this file. Do not attempt to fake storage or retrieval.

## When to ingest

There are three resource shapes a user can share. Use LLM judgement — if the user has not asked you to remember something, do not ingest silently. When intent is ambiguous, ask "Do you want me to remember this?" rather than guessing.

### 1. Attachments

The Telegram adapter has already downloaded the attachment to `<workspace>/.tengu-attachments/<sanitized-filename>` and annotated the incoming message with a line of the form `[Attached file: <absolute-path> (<mime>, <bytes> bytes)]`. You do not need to download anything yourself — the path is already in the user message.

Call:

```
persistent_store
  operation: "store"
  file_path: "<the absolute path from the [Attached file: ...] annotation>"
  description: "<one sentence: topic + source + why the user shared it>"
```

Supported text extraction: `.pdf` (text layer only — image-only PDFs fail with "PDF contains no extractable text"), `.docx`, `.xlsx` / `.xls` / `.xlsm`, and any UTF-8 text file. Unknown binary formats store a manifest but zero chunks — tell the user if that happens.

### 2. URLs

When a user shares a link and clearly wants it remembered or used for a later question:

1. `http_request` with the URL the user gave you (method `GET`). Do not invent URLs.
2. `write_file` the response body to `<workspace>/.tengu-attachments/url-<short-slug>.txt` (or `.md` if the response is clearly markdown).
3. `persistent_store` `store` on that file path with a `description` summarising the page topic and why it matters.

Soft preference: if the response is HTML, strip obvious boilerplate (nav, footer, script tags) before writing. The UTF-8 fallback path stores raw bytes, so noisy HTML becomes noisy chunks. Don't block on perfect cleanup — good enough is fine.

### 3. Pasted text

When the user pastes a long block of text and asks you to remember it, write it to `<workspace>/.tengu-attachments/note-<unix-timestamp>.txt` and call `persistent_store` `store` on that path. Always include a `description`.

## When NOT to ingest

- Casual conversation, greetings, follow-up questions.
- Your own previous answers.
- Anything the user hasn't asked you to keep.
- Short links that are clearly one-off references ("look at this real quick") rather than reference material.

## Why the `description` field matters

On `store`, the description is prepended to the first chunk as `[file:NAME | DESCRIPTION] <chunk text>` before embedding. A specific description materially improves later semantic retrieval. Good shape: one sentence stating the topic, the source, and why it was shared. Bad: "file", "document", the filename repeated.

## Answering questions from stored resources

When the user asks something that might be answered by previously-stored material:

```
persistent_store
  operation: "search"
  query: "<semantic rephrasing of the user's question — dense topic phrase, not the raw short question>"
  top_k: 5
```

Use `top_k: 10` for broad or survey-style questions. Results are deduplicated by `file_id` — each entry is the best-matching chunk from a distinct file — and include `file_id`, `file_name`, `score`, and `matched_chunk`. Cite `file_name` in your answer so provenance is traceable to the user.

If nothing crosses a plausible relevance bar, say so honestly. Do not fabricate answers from thin search results, and do not claim memory contains material that never matched.

## Managing stored resources

- `persistent_store` `operation: "list"` — enumerate stored files (file_id, file_name, size, chunks, stored_at, description).
- `persistent_store` `operation: "delete"`, `file_id: "<id>"` — destructive. Only on explicit user request, and only after confirming which file they mean. If the user says "delete that" without naming one, `list` first and ask them to point at one.

## Honest limitations

- **Image-only PDFs** have no extractable text layer. Tell the user OCR is needed and do not pretend to have stored the content.
- **Chunking is character-based**, not token-aware. Defaults: 1000 chars per chunk, 200 chars overlap (see `[memory] persistent_store_chunk_size` / `persistent_store_chunk_overlap`). Very dense or code-heavy material may split awkwardly at token boundaries.
- **HTML stored via `http_request` → `write_file`** is raw markup unless you strip boilerplate. Chunks will contain noise like `<script>` tags and nav menus.
- **Memory is per-agent-workspace**, shared across every user who talks to that agent. It is not per-Telegram-user. Don't store anything one user wouldn't want another to find via search.
- **Backend is configurable** (`disk` or `qdrant`), but the tool interface is identical — your behavior does not change by backend.

## Enabling this skill

The feature requires `[memory]` to be enabled and `persistent_store` listed in the agent's `workspace_tools`. Example:

```toml
[memory]
enabled = true
# defaults shown — override only if needed
# persistent_store_chunk_size = 5000
# persistent_store_chunk_overlap = 200
# backend = "disk"   # or "qdrant"

[agents.my_agent]
workspace_tools = ["persistent_store"]
skill_packages = ["telegram-rag-ingest"]
```

On disk, chunk manifests live at `<workspace>/.tengu/storage/<file_id>/manifest.json` alongside the raw file, and vector entries are written to the configured memory store.
