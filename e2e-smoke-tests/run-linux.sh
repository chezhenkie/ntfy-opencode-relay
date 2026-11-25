#!/usr/bin/env bash
# Linux wrapper for the ntfy end-to-end smoke.
# Resolves node and hands every option to smoke.mjs unchanged.
# Exit codes: 0 all checks passed, 1 a check failed, 2 node or setup missing.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
smoke="$here/smoke.mjs"

if [[ ! -f "$smoke" ]]; then
  echo "FAIL script $smoke"
  exit 2
fi

node_bin="${NTFY_NODE:-}"
if [[ -z "$node_bin" ]]; then
  node_bin="$(command -v node || true)"
fi
if [[ -z "$node_bin" || ! -x "$node_bin" ]]; then
  echo "FAIL node not found (set NTFY_NODE)"
  exit 2
fi

exec "$node_bin" "$smoke" "$@"
