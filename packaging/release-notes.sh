#!/bin/sh
# Usage: release-notes.sh <tag> <owner/repo> [previous-tag]
# Prints the CHANGELOG.md section of <tag>, then a "Full Changelog" compare link.
set -e
TAG="$1"; REPO="$2"; PREV="$3"
VERSION="${TAG#v}"
cd "$(dirname "$0")/.."

# Section of this version: from "## [X.Y.Z]" up to the next "## [" heading,
# without the link-reference lines at the bottom of the file.
NOTES="$(awk -v v="$VERSION" '
  /^## \[/ { in_section = (index($0, "## [" v "]") == 1); next }
  in_section { print }
' CHANGELOG.md | grep -v '^\[[^]]*\]: ' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')"

if ! printf '%s' "$NOTES" | grep -q '[^[:space:]]'; then
    echo "CHANGELOG.md has no notes for $VERSION" >&2
    exit 1
fi

printf '%s\n\n' "$NOTES"
if [ -n "$PREV" ]; then
    echo "**Full Changelog**: https://github.com/$REPO/compare/$PREV...$TAG"
else
    echo "**Full Changelog**: https://github.com/$REPO/commits/$TAG"
fi
