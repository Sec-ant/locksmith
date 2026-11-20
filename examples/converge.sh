#!/bin/sh
# Run with pnpm 10.33.2, Git, and locksmith on PATH.
set -eu

fixture=$(CDPATH= cd -- "$(dirname "$0")/converge" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cp -R "$fixture/before/." "$scratch/"
cd "$scratch"

install() {
  CI=true pnpm install --ignore-scripts --registry=https://registry.npmjs.org \
    --store-dir "$scratch/store" --cache-dir "$scratch/cache" "$@" >&2
}

show_diff() {
  printf '\n%s\n' "$1"
  git --no-pager diff --numstat HEAD -- pnpm-lock.yaml
  git --no-pager diff --unified=0 HEAD -- pnpm-lock.yaml
}

# Each unchanged site uses a distinct React and Zustand combination.
number=0
for react in 17.0.2 18.2.0 18.3.1; do
  for zustand in 4.3.8 4.4.7 4.5.2 4.5.5; do
    number=$((number + 1))
    site="site-$(printf '%02d' "$number")"
    mkdir -p "apps/$site"
    cat > "apps/$site/package.json" <<JSON
{
  "name": "$site",
  "private": true,
  "dependencies": {
    "react": "$react",
    "zustand": "$zustand"
  }
}
JSON
  done
done

install --frozen-lockfile
git init -q
git add package.json pnpm-workspace.yaml pnpm-lock.yaml apps/*/package.json
git -c user.name=Demo -c user.email=demo@example.org -c commit.gpgsign=false \
  -c core.hooksPath=/dev/null commit -qm baseline

cp "$fixture/installed/apps/editor/package.json" apps/editor/package.json
printf '\n%s\n' 'Manifest edit'
git --no-pager diff HEAD -- apps/editor/package.json

install --no-frozen-lockfile
show_diff 'pnpm install'
cmp "$fixture/installed/pnpm-lock.yaml" pnpm-lock.yaml

locksmith converge HEAD > converged.yaml
cp converged.yaml pnpm-lock.yaml
install --frozen-lockfile --offline
cmp converged.yaml pnpm-lock.yaml
show_diff 'locksmith converge HEAD'
cmp "$fixture/converged/pnpm-lock.yaml" pnpm-lock.yaml
