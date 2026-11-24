---
default: patch
---

Match pnpm's peer declarations for newly added releases. Omit self-peers with pnpm 9.15.1 and later, retain unparseable peer ranges, and record metadata-only optional peers as `*`, including when an ignored optional dependency has the same name.
