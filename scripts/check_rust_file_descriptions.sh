#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

RUST_FILES=()
while IFS= read -r file; do
  RUST_FILES+=("$file")
done < <(find src crates -type f -name '*.rs' | sort)

if ((${#RUST_FILES[@]} == 0)); then
  echo "No Rust files found under src/ or crates/."
  exit 0
fi

MISSING=()

for file in "${RUST_FILES[@]}"; do
  first_non_empty="$(awk 'NF { print; exit }' "$file")"
  if [[ "$first_non_empty" != '//!'* ]]; then
    MISSING+=("$file")
  fi
done

if ((${#MISSING[@]} > 0)); then
  echo "Rust files missing top-of-file description comments (//! ...):"
  for file in "${MISSING[@]}"; do
    echo "  - $file"
  done
  exit 1
fi

echo "All Rust files include a top-of-file description comment."
