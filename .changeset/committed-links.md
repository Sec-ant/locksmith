---
default: patch
---

Resolve linked workspace members and manifests against the selected Git revision, including relative targets containing `.` and `..`. Convergence checks working directory links against the installed lockfile. A conflict in a linked manifest keeps the lockfile unmerged, including when its target filename contains non-UTF-8 bytes.
