#!/usr/bin/env bash
#
# phase-0-checks.sh — Phase 0 pre-flight sanity probes for tengu-cluster.
#
# Non-interactive checks intended to run from the repo root BEFORE the
# Phase 0 interactive smoke tests. Collects all results, prints a summary,
# and exits 0 only if nothing failed. Idempotent — safe to re-run.

set -u

# ---------- help ----------
if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    cat <<'EOF'
phase-0-checks.sh — Phase 0 pre-flight sanity probes for tengu-cluster.

Runs a series of non-interactive checks (toolchain, build, tests compile,
clippy, Qdrant reachability on :6334, git worktree cleanliness, phantom
config.toml/ dir, inventory of agents/skills/sandboxes). Each check prints
a [PASS]/[FAIL]/[WARN]/[INFO] line. Exits 0 if zero FAILs, else 1.

Run from the repo root:  bash scripts/phase-0-checks.sh
EOF
    exit 0
fi

# ---------- counters ----------
PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0
INFO_COUNT=0

pass() { echo "[PASS] $*"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "[FAIL] $*"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
warn() { echo "[WARN] $*"; WARN_COUNT=$((WARN_COUNT + 1)); }
info() { echo "[INFO] $*"; INFO_COUNT=$((INFO_COUNT + 1)); }

# check LABEL COMMAND...  — runs COMMAND, passes if exit 0, fails otherwise.
# On failure, prints the last ~10 lines of combined output, indented.
check() {
    local label="$1"
    shift
    local out rc
    out="$("$@" 2>&1)"
    rc=$?
    if [[ $rc -eq 0 ]]; then
        pass "$label"
        return 0
    else
        fail "$label (exit $rc)"
        if [[ -n "$out" ]]; then
            echo "$out" | tail -n 10 | sed 's/^/       | /'
        fi
        return 1
    fi
}

echo "=== tengu-cluster Phase 0 pre-flight checks ==="
echo "Repo: $(pwd)"
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo

# ---------- 1. cargo available ----------
CARGO_OK=0
if command -v cargo >/dev/null 2>&1; then
    pass "cargo available ($(command -v cargo))"
    CARGO_OK=1
else
    fail "cargo available (not found in PATH)"
fi

# ---------- 2. rustc version ----------
if command -v rustc >/dev/null 2>&1; then
    RUSTC_VER="$(rustc --version 2>&1)"
    pass "rustc version: ${RUSTC_VER}"
else
    fail "rustc version (rustc not found)"
fi

# ---------- 3. cargo build --release ----------
if [[ $CARGO_OK -eq 1 ]]; then
    check "cargo build --release" cargo build --release
else
    fail "cargo build --release (skipped: cargo unavailable)"
fi

# ---------- 4. cargo test --no-run ----------
if [[ $CARGO_OK -eq 1 ]]; then
    check "cargo test --no-run (compile-only)" cargo test --no-run
else
    fail "cargo test --no-run (skipped: cargo unavailable)"
fi

# ---------- 5. cargo clippy (strict) ----------
if [[ $CARGO_OK -eq 1 ]]; then
    check "cargo clippy --all-targets -- -D warnings" \
        cargo clippy --all-targets -- -D warnings
else
    fail "cargo clippy (skipped: cargo unavailable)"
fi

# ---------- 6. qdrant reachable on :6334 ----------
QDRANT_URL="http://localhost:6334/collections"
if command -v curl >/dev/null 2>&1; then
    QDRANT_RESP="$(curl -sS --max-time 3 "$QDRANT_URL" 2>&1)"
    QDRANT_RC=$?
    if [[ $QDRANT_RC -eq 0 && -n "$QDRANT_RESP" ]]; then
        pass "qdrant reachable on :6334"
        echo "       | response: $(echo "$QDRANT_RESP" | head -c 300)"
    else
        fail "qdrant reachable on :6334 (curl exit $QDRANT_RC)"
        if [[ -n "$QDRANT_RESP" ]]; then
            echo "       | $QDRANT_RESP" | head -n 3
        fi
    fi
else
    fail "qdrant reachable on :6334 (curl not installed)"
fi

# ---------- 7. qdrant on :6333 (info only) ----------
if command -v curl >/dev/null 2>&1; then
    ALT_RESP="$(curl -sS --max-time 2 http://localhost:6333/collections 2>/dev/null)"
    ALT_RC=$?
    if [[ $ALT_RC -eq 0 && -n "$ALT_RESP" ]]; then
        warn "qdrant also reachable on :6333 (alternate port — tengu uses :6334)"
    else
        info "qdrant on :6333 not reachable (expected — tengu uses :6334)"
    fi
fi

# ---------- 8. git worktree status ----------
if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    GIT_STATUS="$(git status --short 2>/dev/null)"
    if [[ -z "$GIT_STATUS" ]]; then
        pass "git worktree clean"
    else
        # Allow-list of expected Phase 0 paths.
        UNEXPECTED=""
        while IFS= read -r line; do
            # line format: "XY path"; strip the two-char status + space
            path="${line:3}"
            # handle rename arrows "old -> new"
            path="${path##* -> }"
            case "$path" in
                src/config.rs|config.example.toml|docs/manual-test-checklist.md|scripts/phase-0-checks.sh)
                    ;;
                *)
                    UNEXPECTED+="$line"$'\n'
                    ;;
            esac
        done <<< "$GIT_STATUS"

        if [[ -z "$UNEXPECTED" ]]; then
            pass "git worktree status acceptable (only expected Phase 0 files dirty)"
        else
            warn "git worktree has unexpected dirty files:"
            echo "$UNEXPECTED" | sed 's/^/       | /' | sed '/^[[:space:]]*|[[:space:]]*$/d'
        fi
    fi
else
    warn "git worktree status (not a git repo or git unavailable)"
fi

# ---------- 9. phantom config.toml/ directory ----------
if [[ -d config.toml ]]; then
    warn "phantom directory config.toml/ present — suggest: rmdir config.toml"
else
    info "no phantom config.toml/ directory"
fi

# ---------- 10. agent specs ----------
AGENT_COUNT=0
if [[ -d agents ]]; then
    # shellcheck disable=SC2012
    AGENT_COUNT=$(ls -1 agents/*.toml 2>/dev/null | wc -l | tr -d ' ')
fi
info "agent specs found: ${AGENT_COUNT} (agents/*.toml)"

# ---------- 11. skills ----------
SKILL_COUNT=0
if [[ -d skills ]]; then
    SKILL_COUNT=$(find skills -type f -name SKILL.md 2>/dev/null | wc -l | tr -d ' ')
fi
info "skills found: ${SKILL_COUNT} (skills/**/SKILL.md)"

# ---------- 12. existing sandboxes ----------
if [[ -d sandboxes ]]; then
    SANDBOX_LIST="$(ls -1 sandboxes/*/config.toml 2>/dev/null)"
    if [[ -n "$SANDBOX_LIST" ]]; then
        SANDBOX_COUNT=$(echo "$SANDBOX_LIST" | wc -l | tr -d ' ')
        info "existing sandboxes: ${SANDBOX_COUNT}"
        echo "$SANDBOX_LIST" | sed 's/^/       | /'
    else
        info "existing sandboxes: 0"
    fi
else
    info "existing sandboxes: 0 (no sandboxes/ dir)"
fi

# ---------- summary ----------
echo
echo "=== SUMMARY ==="
echo "Passed:  ${PASS_COUNT}"
echo "Failed:  ${FAIL_COUNT}"
echo "Warned:  ${WARN_COUNT}"
echo "Info:    ${INFO_COUNT}"

if [[ $FAIL_COUNT -eq 0 ]]; then
    exit 0
else
    exit 1
fi
