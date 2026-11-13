#!/bin/sh
# Download the EEST fixtures that yevm-reth/tests/eest.rs runs into tmp/eest/<tag>-develop/.
#
# v5.4.0 is the final Osaka release. Its "stable" build was filled only up to Prague
# (--until=Prague) and has no Osaka cases, so this fetches the "develop" build
# (--until=BPO4): ~420 MB download, ~8 GB unpacked.
set -eu

TAG=${EEST_TAG:-v5.4.0}
cd "$(dirname "$0")/.."
DEST=tmp/eest/$TAG-develop

if [ -d "$DEST/fixtures" ]; then
    echo "$DEST/fixtures already present"
    exit 0
fi

mkdir -p "$DEST"
URL=https://github.com/ethereum/execution-spec-tests/releases/download/$TAG/fixtures_develop.tar.gz
curl -fL --retry 3 "$URL" -o "$DEST/fixtures_develop.tar.gz"
tar -xzf "$DEST/fixtures_develop.tar.gz" -C "$DEST"
rm "$DEST/fixtures_develop.tar.gz"
grep -E '^(ref|commit|command_line_args)' "$DEST/fixtures/.meta/fixtures.ini" | cut -c1-200
