#!/bin/sh
# Format/lint/test gate shared by the pre-commit and pre-push hooks
# (see .cargo-husky/hooks/). Usage:
#   check.sh fast  - formatting + lint only, no compilation (pre-commit)
#   check.sh full  - everything, including clippy and the test suites (pre-push)
set -e

mode="${1:-full}"

echo '+cargo fmt --all -- --check'
cargo fmt --all -- --check

for dir in deploy/cdk site; do
  echo "+prettier --check ($dir)"
  (cd "$dir" && npm run format:check)
  echo "+eslint ($dir)"
  (cd "$dir" && npm run lint)
done

if [ "$mode" = "full" ]; then
  echo '+cargo clippy --workspace --all-targets -- -D warnings'
  cargo clippy --workspace --all-targets -- -D warnings
  echo '+cargo test --workspace'
  cargo test --workspace

  for dir in deploy/cdk site; do
    echo "+npm test ($dir)"
    (cd "$dir" && npm test)
  done
fi
