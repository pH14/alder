#!/usr/bin/env bash
# Remove the per-checkout launchd agent for alderd.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd -P)
LAUNCH_AGENTS_DIR=${ALDERD_LAUNCH_AGENTS_DIR:-${LAUNCH_AGENTS_DIR:-$HOME/Library/LaunchAgents}}
PLIST_PATH=${ALDERD_PLIST_PATH:-$LAUNCH_AGENTS_DIR/com.alder.alderd.plist}
LAUNCHCTL=${ALDERD_LAUNCHCTL:-launchctl}

if ! command -v "$LAUNCHCTL" >/dev/null 2>&1; then
  echo "alderd-uninstall: launchctl not found; this script requires macOS" >&2
  exit 1
fi

# An absent or already-unloaded service is the converged state.
"$LAUNCHCTL" unload "$PLIST_PATH" >/dev/null 2>&1 || true
rm -f "$PLIST_PATH"
echo "uninstalled $PLIST_PATH (repo: $REPO_ROOT)"
