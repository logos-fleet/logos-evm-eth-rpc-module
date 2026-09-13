#!/usr/bin/env bash
# Keeps libverifproxy -- and behind it the whole nimbus/Nim closure -- out of
# what this module ships. Nim is pinned at 2.2.4 and libverifproxy needs 2.2.10,
# so the day it is linked in, the Windows leg stops building at all in a repo
# that never mentioned nimbus.
#
# The risk is `external_libraries`, NOT the dependency list. A module dependency
# is consumed as its published LIDL contract whether it is declared required or
# optional -- measured both ways, identical closures -- and an external lib is
# the only kind that forces a real Windows build of the dependency. So this
# asserts on the artifact rather than on how metadata.json is spelled.
#
# Runs from the repo root with the staged tree present (one dir per target).
set -euo pipefail

hits=$(find stage -type f \( -iname '*nimbus*' -o -iname '*verifproxy*' -o -iname '*libverif*' \) 2>/dev/null || true)
if [ -n "$hits" ]; then
  echo "FAIL: nimbus/libverifproxy artifacts reached the shipped tree:" >&2
  echo "$hits" >&2
  echo >&2
  echo "Something is now linking libverifproxy. Check metadata.json for a" >&2
  echo "verifproxy entry under external_libraries -- that is the declaration" >&2
  echo "that pulls a real build of the dependency in." >&2
  exit 1
fi
echo "ok: no nimbus/libverifproxy in the staged tree ($(find stage -type f | wc -l) files scanned)"
