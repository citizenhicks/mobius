test *args:
    cargo test --workspace --all-targets --all-features --locked {{args}}

fmt:
    cargo fmt

complexity:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
