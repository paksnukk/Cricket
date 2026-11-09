#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version (argument "X.Y.Z" or "vX.Y.Z").
# Falls back to the [Unreleased] section, with a note, when the version has no section yet.
set -euo pipefail
cd "$(dirname "$0")/../.."
ver="${1:?usage: release-notes.sh X.Y.Z}"
ver="${ver#v}"
section() {
  awk -v key="$1" '
    /^## \[/ { if (found) exit; if (index($0, "## [" key "]") == 1) { found = 1; next } }
    found { print }
  ' CHANGELOG.md
}
notes=$(section "$ver")
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
  echo "_No CHANGELOG section for $ver yet; these are the entries under Unreleased._"
  echo
  notes=$(section "Unreleased")
fi
# Drop the link-reference lines and leading/trailing blank lines.
printf '%s\n' "$notes" | sed -e '/^\[.*\]: http/d' | awk 'NF { p = 1 } p' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'
