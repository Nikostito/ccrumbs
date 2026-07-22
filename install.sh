#!/usr/bin/env bash
# Installs cctrail as a `cctrail` command available on your PATH.
#
# Usage:
#   ./install.sh          Install core CLI only
#   ./install.sh --tui    Also install the interactive TUI (pulls in textual)

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

EXTRAS=""
if [[ "${1:-}" == "--tui" ]]; then
    EXTRAS="[tui]"
fi

if command -v pipx >/dev/null 2>&1; then
    echo "Installing with pipx (recommended — isolated environment, cctrail on PATH)..."
    pipx install ".${EXTRAS}" --force
else
    echo "pipx not found, falling back to pip install --user."
    echo "(Consider installing pipx for cleaner isolation: https://pipx.pypa.io)"
    python3 -m pip install --user ".${EXTRAS}"
fi

echo ""
echo "Done. Try:"
echo "  cctrail index"
echo "  cctrail stats"
echo "  cctrail search <keywords>"
if [[ -z "$EXTRAS" ]]; then
    echo ""
    echo "(TUI not installed — re-run './install.sh --tui' to add 'cctrail tui'.)"
fi
