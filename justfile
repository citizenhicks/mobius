test *args:
    cargo test --workspace --all-targets --all-features --locked {{args}}

fmt:
    cargo fmt

complexity:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    swift-complexity mobius-app/apple/Sources mobius-app/apple/Tests --recursive --threshold 21 --report-suppressions
