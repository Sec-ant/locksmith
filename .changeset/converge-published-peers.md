---
default: patch
---

# Record the peers of newly added releases as pnpm does

When `converge` adds a published release, it reads the release's dependencies with the same rules as `generate`. With pnpm 9.15.1 and later, a package's own name is no longer one of its peers. An optional peer declared only in `peerDependenciesMeta` is recorded with the range `*`. A published peer whose range pnpm cannot parse stays a peer.
