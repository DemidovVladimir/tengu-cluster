# Adaptive AI Learning Marketplace — PRD

**Version:** 1.0  
**Date:** April 2026  
**Status:** Draft  
**Author:** Vladimir / Claude

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Problem Statement](#2-problem-statement)
3. [Vision & Goals](#3-vision--goals)
4. [Stakeholders & User Roles](#4-stakeholders--user-roles)
5. [System Architecture](#5-system-architecture)
6. [Core Features](#6-core-features)
7. [Technical Requirements](#7-technical-requirements)
8. [Marketplace Model](#8-marketplace-model)
9. [Development Roadmap](#9-development-roadmap)
10. [Open Questions](#10-open-questions)
11. [Definition of Done — v1.0](#11-definition-of-done--v10)

---

## 1. Executive Summary

The Adaptive AI Learning Marketplace is a platform where course providers upload learning materials (videos, PDFs, articles, audio) and receive an AI-powered agent that teaches their curriculum to learners. Each agent continuously self-evaluates, diagnoses gaps, and adapts the learning path in real time — so every learner always gets the right next step at the right difficulty level.

**For course providers:** upload once, sell forever. The agent handles teaching, evaluation, and adaptation.  
**For learners:** no wasted time on material too easy or too hard. The agent always meets you where you are.  
**For the platform:** a marketplace where agent quality is measurable, improvable, and monetisable.

---

## 2. Problem Statement

### 2.1 The broken state of online learning

Online courses have a well-documented completion rate under 15%. The root cause is not lack of motivation — it is lack of adaptation. A static course cannot know when a learner is stuck, cannot diagnose why, and cannot find better material to replace the failing explanation. The result is learners who plateau, disengage, and abandon.

### 2.2 What providers need

- A way to package expertise and materials into a product that scales without support overhead
- Measurable proof that learners are progressing (for marketing, retention, and iteration)
- Infrastructure that handles evaluation, adaptation, and memory so providers focus on content, not delivery

### 2.3 What learners need

- A teacher that meets them at their exact current level — not a fixed starting point
- Immediate, specific feedback: not just "wrong" but "here is what you misunderstood and here is a better explanation"
- Multiple modalities: video watch, reading, pronunciation drill, hands-on task, quiz — matched to the concept
- Persistent memory: the agent remembers what the learner knows and has tried, across sessions

---

## 3. Vision & Goals

### 3.1 Vision

A marketplace where any expert can turn their knowledge and materials into a living, adaptive AI tutor — and any learner can buy access to a personal teacher that evolves with them indefinitely.

### 3.2 Goals for v1.0

| Goal | Metric | Target |
|------|--------|--------|
| Learner progression | % of learners advancing past module 1 | ≥ 70% |
| Adaptation accuracy | Gap diagnosis correct (blind eval) | ≥ 80% |
| Provider onboarding | Time from upload to live agent | < 30 min |
| Session retention | Learners returning for ≥ 3 sessions | ≥ 60% |
| Autoresearch quality | Replacement material rated useful by learner | ≥ 75% |

---

## 4. Stakeholders & User Roles

| Role | Who they are | Primary need |
|------|-------------|--------------|
| **Course Provider** | Subject-matter experts, coaches, educators, YouTubers | Upload material, configure agent, track learner progress, earn revenue |
| **Learner** | Anyone purchasing a course agent to learn a specific skill | Personalised pacing, honest evaluation, never wasting time on wrong-level content |
| **Platform Admin** | Marketplace operator (us) | Quality control, billing, agent performance monitoring, provider onboarding |
| **Agent (AI)** | The Claude-powered teaching agent created per course | Execute learning path, evaluate learner, adapt path, maintain memory |

---

## 5. System Architecture

### 5.1 High-level overview

The platform has three main layers: a **Provider Portal** (upload + configure), a **Managed Agent Runtime** (Claude Managed Agents API with persistent sessions and memory), and a **Learner Frontend** (web app that renders each step type — video, quiz, write task, pronunciation drill — from the agent's structured output).

### 5.2 Components

| Layer | Components |
|-------|-----------|
| **Provider Portal** | Material upload (video/PDF/URL/audio), goal definition, agent configuration, analytics dashboard |
| **Path Builder (Skill)** | Analyses uploaded resources → generates structured WATCH / READ / QUIZ / WRITE / SPEAK path with exact references and timestamps |
| **Managed Agent Runtime** | Claude claude-sonnet-4-6 agent per course; persistent sessions; tools: web search for autoresearch; memory store per learner |
| **Path Adapter (Skill)** | Autoresearch loop: diagnose gap → search better material → build replacement step → update path |
| **Learner Frontend** | Renders each step type correctly; streams agent responses; captures quiz answers, written submissions, pronunciation attempts |
| **Memory / State** | Per-learner profile: skills mastered, gaps found, materials tried, session history — persists across sessions |

### 5.3 Agent creation flow

1. Provider uploads materials and sets a learning goal
2. Platform runs the **Path Builder skill** → produces a structured learning path (Markdown)
3. Platform calls Anthropic Managed Agents API to create a new agent with the path embedded in its system prompt
4. Agent is deployed to an environment with unrestricted networking (required for autoresearch web access)
5. A unique `agent_id` is issued to the provider; learners purchase access and get sessions against it

---

## 6. Core Features

### 6.1 Path Builder

The Path Builder is a Cowork skill (`SKILL.md`) that takes a learning goal plus a set of resources and produces a structured, typed learning path. It runs once when a provider creates a course.

**Supported resource types:**

- **YouTube video** — fetches description, extracts chapters, assigns timestamps to steps
- **Local video/audio file** (MP4, MKV, M4A, MP3, WAV) — extracts audio with `ffmpeg`, transcribes with Whisper, segments by topic
- **Article or documentation URL** — fetches content, identifies sections, links to anchors
- **PDF or book** — reads file, identifies chapter/page ranges, assigns steps to page spans
- **No resource** — falls back to web search to find equivalent free material, documents substitution

**Step types:**

| Type | What it is | UI renders |
|------|-----------|------------|
| `WATCH` | Specific video segment | Embedded player at exact timestamp |
| `READ` | Article section or PDF page range | Link opened to anchor/page |
| `QUIZ` | Agent-generated questions on preceding content | Interactive Q&A form, scored ≥ threshold |
| `WRITE` | Hands-on task: build, code, or draft something | Text input or file upload |
| `SPEAK` | Pronunciation or verbal exercise | Phonetic guide + Forvo link + mic prompt |

### 6.2 Autoresearch Loop (Path Adapter)

Inspired by Karpathy's autoresearch pattern: **teach → evaluate → identify gap → research better material → replace step → retry**. Triggered automatically when a learner fails a QUIZ below threshold, explicitly says they don't understand, or fails the same concept three times.

**Loop steps:**

1. **Diagnose the real gap** — not just "quiz failed" but *why*: wrong concept, missing prerequisite, wrong format, language barrier
2. **Search for better material** — first in other segments of existing resources, then web search, then free reference sources (MDN, Khan Academy, Forvo, etc.)
3. **Build replacement step(s)** — four options: direct swap, insert prerequisite, change step type (e.g. WATCH → WRITE), or break into smaller steps
4. **Update the path** — replaced step is marked `[ADAPTED - Iteration N]`; an ADAPTATION LOG is prepended
5. **Learner retries** — with the new material

**Escalation rule:** if the same concept fails 3+ times across different materials, the agent flags it explicitly (`⚠ ESCALATION`), suggests a live tutor or community resource, and inserts a WRITE step asking the learner to explain their understanding in their own words — this usually reveals the real misconception.

### 6.3 Learner Memory

Each learner has a persistent profile stored across sessions, implemented using Anthropic Memory Stores (Managed Agents research preview) with a context-injection fallback until access is granted.

- **Skills mastered:** list of step objectives passed with score and date
- **Gaps identified:** concepts that triggered adaptation, with attempt count
- **Materials tried:** sources already used (avoids repeating the same failed material)
- **Learning style notes:** inferred preferences (e.g. prefers video over text, responds well to examples first)
- **Session continuity:** last active step, path version, total time on platform

### 6.4 Provider Portal

- Material upload: drag-and-drop video, PDF, URLs, audio files
- Goal definition: what the learner will be able to DO by the end (not just "understand")
- Agent preview: run a test session against the generated agent before publishing
- Analytics: per-learner progress, step pass rates, most common gap points, adaptation frequency
- Revenue: pricing tiers (one-time purchase, subscription, pay-per-session)

### 6.5 Learner Frontend

- Session-based chat UI powered by Managed Agents streaming API
- Step-aware rendering: each step type renders its appropriate UI component
- Progress tracker: visual path showing completed, current, and upcoming steps
- Manual override: learner can skip a step, mark it as already known, or request a harder version
- Session persistence: resume exactly where you left off, across devices

---

## 7. Technical Requirements

### 7.1 Agent runtime

- **Model:** `claude-sonnet-4-6`
- **Environment:** Anthropic Managed Agents cloud, `networking: unrestricted` (required for autoresearch web access)
- **Sessions API:** `sessions.create(agent_id, environment_id)` → `sessions.events.stream()` + `sessions.events.send()`
- **Tools available to agent:** `agent_toolset_20260401` (includes web search, code execution)
- **Memory:** Anthropic Memory Stores (pending research preview); interim: context injection

### 7.2 Path Builder skill

- Runs inside Cowork (Claude desktop) or as an API call during provider onboarding
- Output format: structured Markdown parsed by the delivery layer
- Local video processing: `ffmpeg` (audio extraction) + OpenAI Whisper (transcription, model: `small`)
- Fallback for blocked URLs: web search for equivalent free material, substitution documented in path output

### 7.3 Data storage

- **Agent configurations:** `agent_id` + `environment_id` per course, stored in platform DB
- **Learner sessions:** `session_id` per learner per course, indexed for resumption
- **Learning paths:** versioned Markdown, updated on each adaptation iteration
- **Adaptation logs:** full history of gap diagnoses and material replacements per learner

### 7.4 Security & privacy

- API keys stored encrypted server-side, never exposed to learner clients
- Learner memory profiles stored per-user, not shared across learners or providers
- Providers retain rights to uploaded materials; platform licence is delivery-only

---

## 8. Marketplace Model

### 8.1 Provider pricing

| Tier | What providers pay | What they get |
|------|--------------------|--------------|
| **Starter** | Free (platform covers API cost up to 1,000 sessions/month) | 1 course agent, basic analytics |
| **Pro** | Revenue share: 20% of learner payments | Unlimited agents, full analytics, priority support |
| **Enterprise** | Fixed monthly fee + 10% revenue share | White-label, custom domain, SLA, dedicated infra |

### 8.2 Learner pricing

- **One-time purchase:** pay once, unlimited sessions for a specific course agent
- **Subscription:** monthly fee for access to a provider's full catalogue of agents
- **Pay-per-session:** micropayment per session for casual learners

### 8.3 Quality signals

Because every adaptation is logged, the platform surfaces objective quality metrics for each course agent: average adaptation rate (lower = better initial material), escalation rate (how often human help is needed), and learner completion rate. These become discovery signals in the marketplace.

---

## 9. Development Roadmap

### Phase 1 — Foundation ✅ (current)

- [x] Path Builder skill: builds structured path from any resource type
- [x] Path Adapter skill: autoresearch loop for stuck learners
- [x] Managed Agent: `create_agent.py` + `run_session.py` working
- [x] Local video processing: `ffmpeg` + Whisper pipeline added to Path Builder

### Phase 2 — Delivery layer

- [ ] Web UI with step-type-aware rendering (WATCH / READ / QUIZ / WRITE / SPEAK)
- [ ] Session streaming via Managed Agents events API
- [ ] Learner memory injection (context-based until Memory Stores GA)
- [ ] Provider portal: material upload + path preview

### Phase 3 — Marketplace

- [ ] Provider onboarding: self-serve agent creation with payment integration
- [ ] Learner accounts: purchase, session history, progress dashboard
- [ ] Analytics: per-agent quality metrics, adaptation heat maps
- [ ] Memory Stores integration (pending Anthropic research preview access)

### Phase 4 — Scale

- [ ] Multi-language support: SPEAK step improvements, Forvo integration, native audio comparison
- [ ] Agent marketplace: discovery, ratings, quality signals
- [ ] Provider tools: A/B testing of materials, bulk upload, API for programmatic course creation
- [ ] Mobile app: offline step caching, push notifications for session reminders

---

## 10. Open Questions

| Question | Options / current thinking | Owner |
|----------|---------------------------|-------|
| How do we handle providers uploading copyrighted video they don't own? | Accept only provider-owned content; add ToS check on upload | Legal / Platform |
| Memory Stores access — when do we get it? | Applied for research preview; context injection workaround in place | Vladimir |
| Which web framework for the learner frontend? | Next.js (SSR + streaming) vs React SPA; leaning Next.js for SEO | Engineering |
| How do we validate SPEAK step quality without human review? | Phoneme matching via speech-to-text diff; crowdsource community ratings | Product |
| Provider API vs portal-only onboarding for v1? | Portal-only for v1; API in Phase 3 for power users | Product |

---

## 11. Definition of Done — v1.0

Version 1.0 is shippable when all of the following are true:

1. A course provider can upload materials, see a generated learning path, and have a live agent session within **30 minutes**
2. The autoresearch loop correctly replaces a failing step with better material in **≥ 80%** of test cases
3. A learner can complete a full module (3–6 steps including a QUIZ) in a single session without manual developer intervention
4. Learner memory persists correctly across at least **3 separate sessions**
5. Local video files (MP4) are transcribed and produce timestamped WATCH steps automatically
6. The platform handles at least **10 concurrent learner sessions** without degradation

---

### Next immediate actions

1. Build the Managed Agents web UI with step-type-aware rendering
2. Wire Path Builder output → agent system prompt → session delivery
3. Test full loop: provider uploads → path built → learner stuck → path adapted → learner succeeds
4. Follow up on Anthropic Memory Stores research preview access
