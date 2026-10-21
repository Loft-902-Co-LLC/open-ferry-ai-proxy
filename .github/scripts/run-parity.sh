#!/usr/bin/env bash
# Offline only: no proxy, provider, credentials, or live-mode arguments.
set -euo pipefail

if [[ ! $PARITY_SEED =~ ^[0-9]+$ || ! $PARITY_RANDOM =~ ^[0-9]+$ ]]; then
  echo '::error::Parity seed and random count must be nonnegative integers'
  exit 2
fi

mkdir -p target/parity
expected=$(git ls-tree HEAD reference/cliproxyapi | awk '{print $3}')
upstream=$(git -C reference/cliproxyapi rev-parse HEAD)
{
  echo '## Offline differential parity'
  echo
  echo "open-ferry commit: \`$GITHUB_SHA\`"
  echo "CLIProxyAPI commit: \`$upstream\` (gitlink: \`$expected\`)"
  echo "Seed: \`$PARITY_SEED\`; random count per generator: \`$PARITY_RANDOM\`."
  echo 'Accepted known differences: `Case::known_difference` in `tools/parity/src/cases.rs`, `tools/parity/src/cases/**` and `tools/parity/src/interactions/**/cases.rs`.'
  echo 'Equivalence rules: `tools/parity/src/compare.rs` and `UPSTREAM.md`.'
} | tee target/parity/ci-metadata.md
cat target/parity/ci-metadata.md >> "$GITHUB_STEP_SUMMARY"
if [[ -z $expected || $upstream != "$expected" ]]; then
  echo '::error::The upstream checkout does not match the pinned gitlink'
  exit 2
fi

started=$SECONDS
set +e
cargo run --locked -p open-ferry-parity -- \
  --upstream reference/cliproxyapi --seed "$PARITY_SEED" --random "$PARITY_RANDOM" \
  --summary target/parity/ci-results.md 2>&1 | tee target/parity/ci.log
statuses=("${PIPESTATUS[@]}")
set -e
status=${statuses[0]}
if [[ ${statuses[1]} != 0 ]]; then status=${statuses[1]}; fi
if [[ -f target/parity/ci-results.md ]]; then
  cat target/parity/ci-results.md | tee -a "$GITHUB_STEP_SUMMARY"
else
  echo 'Parity stopped before producing suite totals; see the log.' | tee -a "$GITHUB_STEP_SUMMARY"
fi
echo "Parity build and comparison: $((SECONDS - started)) seconds; exit status: $status." | tee -a "$GITHUB_STEP_SUMMARY"
exit "$status"
