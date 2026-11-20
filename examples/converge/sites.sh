#!/bin/sh
set -eu

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
