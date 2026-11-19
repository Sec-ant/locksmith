#!/bin/sh
# Run from an isolated workspace containing the lockfile under test.
set -eu

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cp pnpm-lock.yaml "$scratch/before"
if [ ! -f pnpm-workspace.yaml ]; then
  set -- --ignore-workspace "$@"
fi

CI=true "${PNPM_BIN:-pnpm}" install --frozen-lockfile --offline --ignore-scripts \
  --modules-dir "node_modules/.locksmith-check-${scratch##*/}" \
  --config.enable-modules-dir=false --config.optimistic-repeat-install=false "$@"

if ! cmp -s "$scratch/before" pnpm-lock.yaml; then
  echo 'frozen install changed pnpm-lock.yaml' >&2
  exit 1
fi
