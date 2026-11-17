# apply_patch scenarios

Copied from OpenAI Codex, `codex-rs/apply-patch/tests/fixtures/scenarios` at commit
a11d2403a5c8f346527379ccd061c2aa95e7edc4 (Apache-2.0). Each scenario has `input/`,
`patch.txt` and the expected final `expected/` tree.

Mobius rejects a patch before its first write where Codex writes part of it, so three
scenarios end with their input unchanged; `apply_patch.rs` lists them.
