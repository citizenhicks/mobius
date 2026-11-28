test *args:
    cargo test --all-targets --all-features --locked {{args}}

fmt:
    cargo fmt

complexity:
    cargo clippy --all-targets --all-features --locked -- -D warnings

rules:
    sh .github/rules.sh
    sh apps/macos/scripts/rules.sh
