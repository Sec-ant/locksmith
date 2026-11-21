#!/bin/sh
set -eu

for file in .changeset/*.md .changeset/.*.md; do
    [ -f "$file" ] || continue
    awk '
        { sub(/\r$/, "") }
        NR == 1 { front = /^[[:blank:]]*---[[:blank:]]*$/; next }
        !body { body = front && /^[[:blank:]]*---[[:blank:]]*$/; next }
        !NF { next }
        !line { line = NR; summary = $0; next }
        { details = 1 }
        END {
            titled = sub(/^#[[:blank:]]+/, "", summary)
            if (!body) {
                error = "Expected changeset front matter between --- lines."
            } else if (!line || summary ~ /^[#[:space:]]*$/) {
                error = "Write a summary of the user-facing change."
            } else if (summary ~ /^[[:blank:]]*([-+*]|[0-9]+[.)])([[:blank:]]|$)/) {
                error = "Remove the list marker. Knope adds the bullet."
            } else if (details && !titled) {
                error = "Keep the summary on one line. Split independent changes into files or introduce related details with # Short title."
            }
            if (error) {
                print FILENAME ":" (line ? line : 1) ": " error
                exit 1
            }
        }
    ' "$file" >&2
done
