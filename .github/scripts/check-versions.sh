#!/usr/bin/env bash
# Fail unless the workspace Cargo.toml, tauri.conf.json and package.json agree on the version.
set -euo pipefail
cd "$(dirname "$0")/../.."
ws=$(awk '/^\[workspace.package\]/{f=1} f && /^version = /{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)
tc=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' cricket_gui/src-tauri/tauri.conf.json)
pj=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' cricket_gui/package.json)
echo "Cargo.toml [workspace.package] : $ws"
echo "tauri.conf.json                : $tc"
echo "package.json                   : $pj"
if [ -z "$ws" ] || [ "$ws" != "$tc" ] || [ "$ws" != "$pj" ]; then
  echo "error: versions disagree (see docs/release-process.md §2)" >&2
  exit 1
fi
echo "ok: $ws"
