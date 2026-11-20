#!/bin/bash
# Runs .github/workflows/build.yml from here and waits for it: the three builds, the end-to-end test on each
# runner (unless --no-test) and, with --release, the release build-N. Then the binaries, test logs and GUI
# screenshots are in out/ci/<os>/ (to check them, e.g. the GUI in WSL). Push HEAD first: the runners build
# what GitHub has.
# usage: bash scripts/ci.sh [--release] [--no-test]
set -e
RELEASE=false; TEST=true
for a in "$@"; do case "$a" in --release) RELEASE=true;; --no-test) TEST=false;; *) echo "usage: ci.sh [--release] [--no-test]"; exit 2;; esac; done
git fetch -q origin && git merge-base --is-ancestor HEAD "@{u}" || { echo "push HEAD first (git push)"; exit 2; }
BEFORE=$(gh run list -w build.yml -L 1 --json databaseId -q '.[0].databaseId' 2>/dev/null || true)
gh workflow run build.yml -f release=$RELEASE -f test=$TEST > /dev/null
ID=""
for _ in $(seq 1 30); do
  ID=$(gh run list -w build.yml -L 1 --json databaseId -q '.[0].databaseId')
  [ -n "$ID" ] && [ "$ID" != "$BEFORE" ] && break; sleep 2
done
echo "run $ID: $(gh run view "$ID" --json url -q .url)"
STATUS=0; gh run watch "$ID" --exit-status --interval 30 > /dev/null || STATUS=$?
rm -rf out/ci; mkdir -p out/ci
gh run download "$ID" -D out/ci > /dev/null 2>&1 || true
for d in out/ci/gangli-*; do [ -d "$d" ] && mv "$d" "out/ci/${d##*/gangli-}"; done
for os in linux windows macos; do
  [ -f "out/ci/$os/test.log" ] && echo "$os: $(tail -1 "out/ci/$os/test.log")"
done
gh run view "$ID" --json jobs -q '.jobs[] | "  " + .name + ": " + .conclusion'
[ "$RELEASE" = true ] && [ "$STATUS" = 0 ] && gh release list -L 1
exit $STATUS
