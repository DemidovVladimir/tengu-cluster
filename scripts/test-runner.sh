#!/usr/bin/env bash
#
# test-runner.sh — Phase 3 black-box test of the SubprocessRunner IPC boundary.
#
# Builds `tengu run-agent`, pipes a minimal AgentIpcInput JSON into it via
# TENGU_AGENT_IPC=1, and asserts the stdout JSON has status=ok. No orchestrator,
# no LLM, no Qdrant required (compress_and_store write is best-effort — the
# test still passes if Qdrant is down).
#
# Usage:    bash scripts/test-runner.sh [--release]
# Returns:  0 on success, non-zero on failure.

set -u
set -o pipefail

RELEASE_FLAG=""
if [[ "${1:-}" == "--release" ]]; then
    RELEASE_FLAG="--release"
fi

echo "=== Phase 3 test-runner ==="

# 1. Build (quiet unless it fails).
echo "[1/3] cargo build ${RELEASE_FLAG} -q"
if ! cargo build ${RELEASE_FLAG} -q 2>&1; then
    echo "FAIL: cargo build failed"
    exit 1
fi

# 2. Locate the built binary.
if [[ -n "${RELEASE_FLAG}" ]]; then
    BIN="target/release/tengu"
else
    BIN="target/debug/tengu"
fi
if [[ ! -x "${BIN}" ]]; then
    echo "FAIL: binary not found at ${BIN}"
    exit 1
fi

# 3. Construct IPC input and pipe into run-agent.
INPUT='{
  "goal": "say hello and confirm IPC works",
  "agent_name": "researcher",
  "model": "openai/gpt-4o",
  "tools": ["http_request"],
  "skills": ["web-research"],
  "max_turns": 5,
  "sandbox": null,
  "session_id": "test-session-phase3",
  "step_id": "step-1"
}'

echo "[2/3] invoking: TENGU_AGENT_IPC=1 ${BIN} run-agent  (via pipe)"
OUTPUT=$(printf '%s' "${INPUT}" | TENGU_AGENT_IPC=1 "${BIN}" run-agent 2>/tmp/tengu-runner-stderr)
EXIT=$?

if [[ $EXIT -ne 0 ]]; then
    echo "FAIL: run-agent exited ${EXIT}"
    echo "--- stderr ---"
    cat /tmp/tengu-runner-stderr
    exit 1
fi

# 4. Validate the JSON response.
echo "[3/3] validating IPC output"
echo "--- stdout ---"
echo "${OUTPUT}"

if ! echo "${OUTPUT}" | grep -q '"status":"ok"'; then
    echo "FAIL: expected status=ok in stdout"
    exit 1
fi
if ! echo "${OUTPUT}" | grep -q '"summary":'; then
    echo "FAIL: expected summary field in stdout"
    exit 1
fi

echo
echo "PASS: IPC boundary works. Phase 3 subprocess stub is functional."
echo "      (Phase 4 replaces the canned summary with a real LLM mini-loop.)"
