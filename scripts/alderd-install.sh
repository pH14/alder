#!/usr/bin/env bash
# Install or update the per-checkout launchd agent for alderd.
#
# The script does nothing until explicitly run. Re-running it adopts the
# existing label: unload the old plist, replace it atomically, then load the
# rendered plist again.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd -P)
TEMPLATE="$REPO_ROOT/crates/alderd/contrib/com.alder.alderd.plist"

LAUNCH_AGENTS_DIR=${ALDERD_LAUNCH_AGENTS_DIR:-${LAUNCH_AGENTS_DIR:-$HOME/Library/LaunchAgents}}
PLIST_PATH=${ALDERD_PLIST_PATH:-$LAUNCH_AGENTS_DIR/com.alder.alderd.plist}
LAUNCHCTL=${ALDERD_LAUNCHCTL:-launchctl}

if [ ! -f "$TEMPLATE" ]; then
  echo "alderd-install: plist template not found: $TEMPLATE" >&2
  exit 1
fi
if ! command -v "$LAUNCHCTL" >/dev/null 2>&1; then
  echo "alderd-install: launchctl not found; this script requires macOS" >&2
  exit 1
fi

if [ -n "${ALDERD_BIN:-}" ]; then
  ALDERD_BIN_VALUE=$ALDERD_BIN
elif [ -x "$REPO_ROOT/target/release/alderd" ]; then
  ALDERD_BIN_VALUE=$REPO_ROOT/target/release/alderd
elif command -v alderd >/dev/null 2>&1; then
  ALDERD_BIN_VALUE=$(command -v alderd)
else
  echo "alderd-install: alderd not found; set ALDERD_BIN to its path" >&2
  exit 1
fi

# XML-escape path-like values before putting them into the plist. The paths
# are normally simple macOS paths, but a checkout may legally contain spaces
# or XML-significant characters.
xml_escape() {
  local value=$1
  value=${value//&/&amp;}
  value=${value//</&lt;}
  value=${value//>/&gt;}
  value=${value//\"/&quot;}
  value=${value//\'/&apos;}
  printf '%s' "$value"
}

# Escape sed replacement metacharacters after XML escaping. The delimiter is
# | so paths do not need their ordinary slash characters escaped.
sed_escape() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//&/\\&}
  value=${value//|/\\|}
  printf '%s' "$value"
}

repo_root_xml=$(xml_escape "$REPO_ROOT")
alderd_bin_xml=$(xml_escape "$ALDERD_BIN_VALUE")
path_xml=$(xml_escape "${PATH:-/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin}")
repo_root_sed=$(sed_escape "$repo_root_xml")
alderd_bin_sed=$(sed_escape "$alderd_bin_xml")
path_sed=$(sed_escape "$path_xml")

mkdir -p "$(dirname "$PLIST_PATH")"
tmp_plist=$(mktemp "$(dirname "$PLIST_PATH")/.com.alder.alderd.plist.XXXXXX")
cleanup() {
  rm -f "$tmp_plist"
}
trap cleanup EXIT

sed \
  -e "s|@REPO_ROOT@|$repo_root_sed|g" \
  -e "s|@ALDERD_BIN@|$alderd_bin_sed|g" \
  -e "s|@PATH@|$path_sed|g" \
  "$TEMPLATE" >"$tmp_plist"

# launchctl load rejects an already-loaded label. Unloading first makes the
# operation convergent and gives the replacement plist the same label.
"$LAUNCHCTL" unload "$PLIST_PATH" >/dev/null 2>&1 || true
mv -f "$tmp_plist" "$PLIST_PATH"
"$LAUNCHCTL" load "$PLIST_PATH"
trap - EXIT
echo "installed $PLIST_PATH"
