---
default: patch
---

Resolve linked workspace members and manifests against the selected Git revision, including relative targets containing `.` and `..`. On Git 2.38 and later, blobless clones with one promisor remote fetch the selected missing manifests in a single batch. Convergence checks working directory links against the installed lockfile. A conflict in a linked manifest keeps the lockfile unmerged, including when its target filename contains non-UTF-8 bytes.
