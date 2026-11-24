---
default: minor
---

Match npm partial upper bounds such as `<=27`, which include every 27.x release. Rust APIs that take or return a pnpm version now use `nodejs_semver::Version` v7.
