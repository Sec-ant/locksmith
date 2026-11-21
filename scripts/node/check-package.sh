#!/bin/sh
set -eu
target=$(rustc -vV | sed -n 's/^host: //p')
cargo build --release --locked --target "$target" -p locksmith-node -p locksmith-cli
node scripts/node/stage.mjs "$target" "target/$target/release" target/npm
npm pack ./target/npm/root ./target/npm/native --ignore-scripts --pack-destination target/npm --silent
node scripts/node/test-install.mjs target/npm
