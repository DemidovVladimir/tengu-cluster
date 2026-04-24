#!/usr/bin/env bash
#
# phase-0-checks.sh — Phase 0 pre-flight sanity probes for tengu-cluster.
#
# Non-interactive checks to run from the repo root BEFORE the Phase 0
# interactive smoke tests (docs/manual-test-checklist.md). Collects all
# results, prints a summary, exits 0 only if nothing failed. Idempotent.

set -u

# ---------- help ----------
if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    cat <<'EOF'
phase-0-checks.sh — Phase 0 pre-flight sanity probes for tengu-cluster.

Runs a series of non-interactive checks: cargo toolchain, release build,
tests compile, clippy strict, Qdrant reachability on :6334, git worktree
cleanliness, phantom config.toml/ directory, inventory of agents, skills,
and sandboxes. Each check prints a [PASS]/[FAIL]/[WARN]/[INFO] line.
Exits 0 iff zero FAILs.

Run from the repo root:   bash scripts/phase-0-checks.sh
EOF
    exit 0
fi

# ---------- counters ----------
PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0
INFO_COUNT=0

pass() { echo "[PASS] $1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "[FAIL] $1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
warn() { echo "[WARN] $1"; WARN_COUNT=$((WARN_COUNT + 1)); }
info() { echo "[INFO] $1"; INFO_COUNT=$((INFO_COUNT + 1)); }

# run cmd, capture exit code; pass on 0, fail on non-zero
check() {
    local label="$1"; shift
    local out
    if out=$("$@" 2>&1); then
        pass "$label"
        [[ -n "${out}" ]] && echo "       ${out}" | head -3
    else
        fail "$label (exit $?)"
        echo "${out}" | tail -5 | sed 's/^/       /'
    fi
}

echo "=== Phase 0 pre-flight checks ==="
echo

# ---------- 1. cargo available ----------
if command -v cargo >/dev/null 2>&1; then
    pass "cargo available"
    HAVE_CARGO=1
else
    fail "cargo not found on PATH"
    HAVE_CARGO=0
fi

# ---------- 2. rustc version ----------
if [[ $HAVE_CARGO -eq 1 ]]; then
    rustc_ver=$(rustc --version 2>/dev/null)
    if [[ -n "$rustc_ver" ]]; then
        pass "rustc available"
        echo "       ${rustc_ver}"
    else
        warn "rustc version could not be determined"
    fi
fi

# ---------- 3. cargo build --release ----------
if [[ $HAVE_CARGO -eq 1 ]]; then
    echo "--- cargo build --release (this takes a while on first run) ---"
    check "cargo build --release" cargo build --release
else
    fail "cargo build --release  (skipped — cargo missing)"
fi

# ---------- 4. cargo test --no-run ----------
if [[ $HAVE_CARGO -eq 1 ]]; then
    check "cargo test --no-run (compile-only)" cargo test --no-run
else
    fail "cargo test --no-run  (skipped — cargo missing)"
fi

# ---------- 5. cargo clippy strict ----------
if [[ $HAVE_CARGO -eq 1 ]]; then
    check "cargo clippy --all-targets -- -D warnings" \
        cargo clippy --all-targets -- -D warnings
else
    fail "cargo clippy  (skipped — cargo missing)"
fi

# ---------- 6. qdrant :6334 ----------
if curl -sS --max-time 3 http://localhost:6334/collections >/dev/null 2>&1; then
    pass "Qdrant reachable on :6334"
    resp=$(curl -sS --max-time 3 http://localhost:6334/collections 2>/dev/null)
    echo "       ${resp}" | head -c 200; echo
else
    fail "Qdrant NOT reachable on :6334 (start with: docker run -d -p 6333:6333 -p 6334:6334 qdrant/qdrant:latest)"
fi

# ---------- 7. qdrant :6333 (info only) ----------
if curl -sS --max-time 3 http://localhost:6333/collections >/dev/null 2>&1; then
    info "Qdrant also reachable on :6333 (alternate port — not required)"
else
    info "Qdrant not on :6333 (not required; :6334 is the codebase default)"
fi

# ---------- 8. git worktree ----------
if command -v git >/dev/null 2>&1; then
    git_status=$(git status --short 2>/dev/null)
    if [[ -z "$git_status" ]]; then
        pass "git worktree clean"
    else
        # Allow expected Phase 0 paths; warn on anything else
        unexpected=$(echo "$git_status" | grep -vE '(src/adapters/config\.rs|config\.example\.toml|docs/manual-test-checklist\.md|scripts/phase-0-checks\.sh|REDESIGN\.md|docs/IMPLEMENTATION_PLAN\.md|docs/architecture-v2\.md|\.claude/)' || true)
        if [[ -z "$unexpected" ]]; then
            pass "git worktree contains only expected Phase 0 changes"
        else
            warn "git worktree has unexpected changes:"
            echo "$unexpected" | sed 's/^/       /'
        fi
    fi
else
    warn "git not on PATH (skipping worktree check)"
fi

# ---------- 9. phantom config.toml/ ----------
if [[ -d config.toml ]]; then
    warn "phantom empty directory 'config.toml/' present — remove with: rmdir config.toml"
else
    pass "no phantom config.toml/ directory"
fi

# ---------- 10-12. inventory ----------
agent_count=$(find agents -maxdepth 1 -name '*.toml' -type f 2>/dev/null | wc -l | tr -d ' ')
info "agent specs in agents/: ${agent_count} (0 expected in Phase 0, non-zero from Phase 2)"

skill_count=$(find skills -type f -name 'SKILL.md' 2>/dev/null | wc -l | tr -d ' ')
info "skills (SKILL.md files) found: ${skill_count}"

if [[ -d sandboxes ]]; then
    sandbox_list=$(find sandboxes -maxdepth 2 -name 'config.toml' -type f 2>/dev/null | sort)
    sandbox_count=$(echo "$sandbox_list" | grep -c . || true)
    info "existing sandboxes (config.toml): ${sandbox_count}"
    [[ -n "$sandbox_list" ]] && echo "$sandbox_list" | sed 's/^/       /'
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
