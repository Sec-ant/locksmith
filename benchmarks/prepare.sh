#!/bin/sh
# Prepare the inputs once, outside the measured command.
set -eu
bench=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
fixture="$bench/../examples/converge"
cd "$1"
mkdir repository
cd repository
cp -R "$fixture/before/." .
sh "$fixture/sites.sh"

# Replicate one unchanged site so peer drift and workspace discovery span 314 projects.
copies=300
i=1
while [ "$i" -le "$copies" ]; do
  site=$(printf 'site-01-%03d' "$i")
  mkdir "apps/$site"
  sed "s/\"site-01\"/\"$site\"/" apps/site-01/package.json > "apps/$site/package.json"
  i=$((i + 1))
done
expand_lock() {
  awk -v copies="$copies" 'BEGIN { RS = "" }
    NR > 1 { print "" }
    { print }
    $1 == "apps/site-01:" {
      for (i = 1; i <= copies; i++) {
        block = $0
        sub(/apps\/site-01:/, sprintf("apps/site-01-%03d:", i), block)
        print "\n" block
      }
    }' "$1"
}
expand_lock "$fixture/before/pnpm-lock.yaml" > pnpm-lock.yaml
git init -q
git config user.name Benchmark
git config user.email benchmark@example.org
git config commit.gpgsign false
git config core.hooksPath /dev/null
git add .
git commit -qm baseline
git worktree add -q --detach ../worktree HEAD

# Repair must recover a reachable snapshot from this commit's parent.
sed '/^  js-tokens@4\.0\.0: {}$/{ N; d; }' pnpm-lock.yaml > ../damaged.yaml
cp pnpm-lock.yaml ../base.yaml
mv ../damaged.yaml pnpm-lock.yaml
git commit -qam 'Remove the js-tokens snapshot'
git tag missing-snapshot

cd ../worktree
cp "$fixture/installed/apps/editor/package.json" apps/editor/package.json
expand_lock "$fixture/installed/pnpm-lock.yaml" > pnpm-lock.yaml
expand_lock "$fixture/converged/pnpm-lock.yaml" > ../ours.yaml

# Merge independent changes: the editor adds Immer and the workspace root adds React.
add_root_react() {
  awk '$0 == "  .: {}" {
    print "  .:\n    dependencies:\n      react:\n        specifier: 18.2.0\n        version: 18.2.0"
    next
  } { print }' "$1"
}
add_root_react ../base.yaml > ../theirs.yaml
add_root_react ../ours.yaml > ../merged.yaml

# Give the installed graph a real peer mismatch across the replicated sites.
sed "s/immer: '>=9\.0[^']*'/immer: '^11.0.0'/" pnpm-lock.yaml > ../inspect.yaml

export LOCKSMITH_BENCH_DIR
LOCKSMITH_BENCH_DIR=$(cd .. && pwd)
export LOCKSMITH_OUTPUT="$LOCKSMITH_BENCH_DIR/actual.yaml"
"$bench/cli" merge
cmp ../merged.yaml ../actual.yaml
"$bench/cli" converge
cmp ../ours.yaml ../actual.yaml
"$bench/cli" repair
cmp ../base.yaml ../actual.yaml
"$bench/cli" inspect
