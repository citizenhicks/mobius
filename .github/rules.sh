#!/bin/sh
# AGENTS.md rules that Clippy cannot express. Prints each violation as file:line and
# exits non-zero when any remain. Test code (`#[cfg(test)]` items, `*_tests.rs`,
# `tests/`) is exempt, as AGENTS.md exempts it. A line may opt out with a trailing
# `// rules-allow: <reason>` comment that says why the rule does not apply there.
set -eu
cd "$(dirname "$0")/.."

# Production Rust sources, without test files.
sources() {
    find src crates examples -name '*.rs' \
        ! -name '*_tests.rs' ! -name 'tests.rs' ! -path '*/tests/*' ! -path '*_tests/*' \
        ! -path '*/target/*'
}

# Prints file:line:text for production lines matching an extended regex, skipping each
# `#[cfg(test)]` item by following its braces. Patterns use bracket classes, which awk
# keeps literal, rather than backslash escapes.
production_matches() {
    pattern=$1
    shift
    sources | grep -v -E "${1:-^$}" | while read -r file; do
        awk -v pattern="$pattern" -v file="$file" '
            skip == 1 {
                opened += gsub(/\{/, "{"); closed += gsub(/\}/, "}")
                if (opened > 0 && opened == closed) { skip = 0 }
                else if (opened == 0 && $0 ~ /;[[:space:]]*$/) { skip = 0 }
                next
            }
            /^[[:space:]]*#\[cfg\(test\)\]/ { skip = 1; opened = 0; closed = 0; next }
            {
                # Arc::clone, Rc::clone and Weak::clone share a handle, the one sanctioned way to share.
                line = $0
                gsub(/(Arc|Rc|Weak)::clone[(]/, "", line)
            }
            line ~ pattern && $0 !~ /^[[:space:]]*\/\// && $0 !~ /rules-allow: / {
                print file ":" NR ":" $0
            }
        ' "$file"
    done
}

failures=0
report() {
    rule=$1
    found=$2
    if [ -n "$found" ]; then
        printf '%s\n%s\n\n' "$rule" "$found"
        failures=$((failures + 1))
    fi
}

report "No clones in production code (.clone(), .cloned(), Type::clone): borrow, move or take; share handles with Arc::clone(&x)." \
    "$(production_matches '[.]clone[(][)]|[.]cloned[(][)]|::clone[(]')"

report "No hosted-service specifics: Cloud, Cloud Plus, billing or service domains." \
    "$(production_matches '[Mm](ö|o)bius [Cc]loud|[Cc]loud [Pp]lus|[Bb]illing([^_]|$)|thinkingsand[.]dev')"

report "No compatibility code: no serde aliases." \
    "$(production_matches 'serde[(][^)]*alias')"

report "Provider IDs are matched only in provider code." \
    "$(production_matches '"(claude|kimi|deepseek|mistral|chat_gpt|open_router)"' \
        '^src/backend/model/|^crates/mobius-gateway/src/assembly[.]rs$')"

middleware='"(artifacts|attachments|compaction|computer_control|extensions|image_generation|instructions|intelligent_ui|questions|scratchpad|subagents|tasks)"'
report "The terminal frontend renders the catalog and never branches on middleware names." \
    "$(production_matches "$middleware" \
        '^(src|examples|crates/mobius-gateway)/|^crates/mobius-cli/src/[^f]|/frontend/catalog[.]rs$')"

dependencies=$(cargo tree -p mobius -e normal --prefix none --locked)
report "The mobius crate never depends on terminal UI crates." \
    "$(printf '%s\n' "$dependencies" | grep -E '^(ratatui|crossterm) ' || true)"

if [ "$failures" -gt 0 ]; then
    echo "$failures rule(s) failed"
    exit 1
fi
echo "All rules pass"
