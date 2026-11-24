---
default: patch
---

# Record the peers of newly added releases as pnpm does

When Locksmith adds a published release during reconciliation, it records its peer declarations using pnpm’s rules. With pnpm 9.15.1 and later, a package's own name is no longer one of its peers. An optional peer declared only in `peerDependenciesMeta` is recorded with the range `*`. A published peer whose range pnpm cannot parse stays a peer.
