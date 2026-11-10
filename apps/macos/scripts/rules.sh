#!/bin/sh
# Rules that Clippy cannot express. Prints each violation as file:line and exits non-zero
# when any remain. Test code (`#[cfg(test)]` items, `*_tests.rs`, `tests/`) is exempt. A
# line may opt out with a trailing `// rules-allow: <reason>` comment that says why the
# rule does not apply there.
set -eu
cd "$(dirname "$0")/.."

# Production Rust sources, without test files.
sources() {
    find src browser-helper/src crates/*/src -name '*.rs' \
        ! -name '*_tests.rs' ! -name 'tests.rs' ! -path '*/tests/*' ! -path '*_tests/*' \
        ! -path '*/target/*'
}

# Prints file:line:text for production lines matching an extended regex, skipping each
# `#[cfg(test)]` item by following its braces. Patterns use bracket classes, which awk
# keeps literal, rather than backslash escapes.
production_matches() {
    pattern=$1
    sources | while read -r file; do
        awk -v pattern="$pattern" -v file="$file" '
            skip == 1 {
                opened += gsub(/\{/, "{"); closed += gsub(/\}/, "}")
                if (opened > 0 && opened == closed) { skip = 0 }
                else if (opened == 0 && $0 ~ /;[[:space:]]*$/) { skip = 0 }
                next
            }
            /^[[:space:]]*#\[cfg\(test\)\]/ { skip = 1; opened = 0; closed = 0; next }
            {
                # Arc::clone, Rc::clone and Weak::clone share a handle, the one sanctioned way to
                # share. GPUI handles, tokio unbounded senders and CEF browsers count references the
                # same way, so cloning one is sharing too, as do the app types that hold only such
                # handles and Copy fields.
                line = $0
                gsub(/(Arc|Rc|Weak|Entity|WeakEntity|AnyView|SharedString|SharedUri|FocusHandle|ScrollHandle|ListState|AgentScope|Background|Picture|EditInput|Thumbnail|ThumbnailKey|UnboundedSender|Browser)::clone[(]/, "", line)
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

report "No clones in production code (.clone(), .cloned(), Type::clone): borrow, move or take; share handles with Arc::clone(&x) or a GPUI handle's Entity::clone(&x)." \
    "$(production_matches '[.]clone[(][)]|[.]cloned[(][)]|::clone[(]')"

if [ "$failures" -gt 0 ]; then
    echo "$failures rule(s) failed"
    exit 1
fi
echo "All rules pass"
