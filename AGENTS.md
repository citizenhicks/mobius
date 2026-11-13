# Contributing to möbius

möbius is a small framework whose shipped runtime has one headless composition
root. Keep changes local to the module that owns the behavior; do not add adapter
layers, parallel registries, or speculative extension points.

## Ownership

| Path | Owns |
| --- | --- |
| `src/agent/` | The linear session, model, and tool loop |
| `src/backend/model/` | Model transports, provider manifests, and routing |
| `src/backend/sandbox/` | File and command execution boundaries and approval policy |
| `src/backend/checkpoint/` | Durable checkpoints, journals, and session catalog |
| `src/middleware/` | Optional capabilities and their tools, hooks, state, and UI contributions |
| `src/protocol/` | Frontend-neutral operations, events, and presentation records |
| `crates/mobius-gateway/` | Headless composition, auth, Bots, sessions, routines, artifacts, and usage |
| `crates/mobius-cli/src/frontend/` | Thin terminal gateway client and rendering |
| `apps/macos/` | The macOS app, a separate Cargo workspace; see [its AGENTS.md](apps/macos/AGENTS.md) |

`mobius-gateway` is the only shipped owner of an `Agent`. The CLI sends gateway
operations and renders gateway events; capability behavior remains in its framework
module.

## Design rules

- Reuse an existing trait or protocol record before adding a new abstraction.
- Keep seams narrow: the owning module hides its state, validation, policy, and wire
  details behind an existing trait or protocol. Do not introduce forwarding layers
  or make callers coordinate an owner's internal steps.
- Keep editable prompts, tool text, model catalogs, defaults, and provider wire mappings
  in owner-local TOML files, embedded and deserialized into typed data. Maintain one
  source of truth; do not duplicate those values in Rust or build a global config registry.
  Runtime control flow, trust-boundary validation, and internal safety bounds remain Rust.
- Prefer data lookup for static mappings and owner-provided parsing for policy choices.
  Keep exhaustive protocol matches and explicit composition readable; do not hide
  necessary behavior behind a generic rule engine just to reduce branch counts.
- Keep dependencies explicit. Registries assemble objects; they are not service locators.
- Middleware declaration order is observable hook and prompt-section order;
  `session_end` alone unwinds in reverse.
- `prompt_section` is composed once at agent creation; use `pre_model` for dynamic durable state.
- Keep prompt sections short and capability-local. Do not repeat tool schemas or
  eagerly inject full skill instructions.
- Expose meaningful policy as middleware construction options. The middleware owns
  defaults and validation; keep internal safety bounds and implementation constants private.
- Providers advertise neutral capabilities; the owning middleware holds thresholds,
  prompts, and branch policy. Never branch on provider IDs outside provider assembly.
- Provider adapters normalize private wire fields into `ModelEvent` and `ModelOutput`;
  checkpoints and the loop never inspect provider-specific fields.
- A capability owns its commands, widgets, references, event rendering, and tests.
  The TUI renders the catalog and must not branch on middleware names.
- Middleware UI ownership is semantic, not platform-specific. A larger middleware may
  split into `runtime.rs`, `tools.rs`, and `presentation.rs`, but presentation emits only
  frontend-neutral protocol records and actions. Platform views stay in their frontend
  and render presentation types rather than branching on middleware names.
- Removing a provider or middleware means deleting its module and explicit composition
  entry. Adding one provider or tool changes only its module and that registry entry.
- Keep Ratatui and terminal concepts out of `mobius`. Keep provider and sandbox
  details out of the agent loop.
- Validate external data at its boundary and keep sandbox execution fail-closed.
- Check each input once, as early as possible, and fail there. Downstream code trusts the
  value and never re-checks it: no repeated validation along a call chain, no whole-config
  re-validation of unchanged parts, no client copies of gateway rules, and no clients
  re-validating gateway-sent data. When a library entry point needs protection, make the
  single check produce a validated type that only valid data can construct. A later check
  is allowed only for different input: newly arrived data, a hook-rewritten value, state
  observed after waiting on a lock or an await, or a re-read from disk.
- Keep hosted-service specifics out of the framework, gateway and CLI. Behavior that exists
  only for möbius Cloud lives in the Cloud codebase; these crates expose neutral operator
  hooks (access expiry, storage limits, telemetry collectors, endpoints) and never name
  Cloud, plans, subscriptions, billing, or hosted-service domains in code, defaults, tests,
  or operator docs. The exceptions are user-facing README links, privacy disclosures, and
  the macOS app's Cloud sign-in (`apps/macos`).
- Do not `.clone()` in production code. Borrow, move, use `std::mem::take`/`replace`, or
  share an `Arc`/`Rc` handle instead; restructure before reaching for a copy, and name any
  copy that remains unavoidable. Tests may clone.
- Add the smallest behavior-focused test that would catch the change. Only write a test if it would actually fail when the code is broken in a way that matters. Everything else is slop.
- Do not add compatibility code. This project has no legacy contract: no old
  names, aliases, dual reads or writes, fallback state discovery, or migrations.

## Rust style

Follow the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html).
`mobius` (`src/`) is library code, so the guide's API rules bind there. `mobius-gateway`
and `mobius-cli` are application code: prefer the direct solution over a general one.
The rules below are the ones that apply here; the lints in `Cargo.toml` already enforce
some of them. Where another section of this file is stricter, that section wins.

Modules and imports:

- A module that declares submodules holds only `mod` lines, re-exports, and a little
  glue. Implement in leaf modules, and name a leaf after its primary item.
- One module per `use`: no `::` inside braces. No glob imports except `use super::*` in
  an inline test module and `pub use child::*` in the parent that re-exports it.
- Import types and write them unqualified (`Arc`, not `std::sync::Arc`). Call free
  functions through their module (`mem::take`, `fs::read`). Qualify a type only to
  disambiguate it (`io::Error`, `fmt::Result`).
- Import a trait inside the function that calls its methods unless the whole file uses it.

Ownership and failure:

- Borrow first, then move. `Arc` and `Rc` are for state several tasks really own; they
  are not a way around the no-clone rule.
- Never read state out of a poisoned mutex, and do not call `catch_unwind`. A panic is a
  bug: fail the operation and report it.
- Return `Option` when absence is the whole story and `Result` when the caller needs the
  cause. Panic only on a broken invariant.

Signatures and types:

- In the public API of `src/`, return `impl Iterator` when callers only iterate and accept
  `impl IntoIterator` when the function only iterates. Use `impl Into<T>` parameters only
  there, and only where call sites clearly benefit. Application code takes concrete types.
- A `bool` parameter whose meaning is not obvious at the call site becomes a two-variant enum.
- Use named fields instead of tuples and multi-field tuple structs.
- Define a trait only for generic code, `dyn Trait` or `impl Trait`, or an extension trait
  named `FooExt`. A trait with one implementation and no second one planned is an
  inherent `impl`.
- Implement the standard trait rather than a look-alike method (`FromStr`, `Display`,
  `Default`, `From`), and derive rather than hand-write. Options structs with public
  fields implement `Default`.
- In `src/`, mark public enums and public-field structs that can grow `#[non_exhaustive]`,
  and mark a function `#[must_use]` when dropping its result is a bug.

Naming:

- Constructors are `new`, `try_new` when fallible, and `from_foo`. Accessors are `foo()`,
  `foo_mut()`, `set_foo()`, and `take_foo()`, never `get_foo()`.
- Do not abbreviate where a newcomer would have to look the word up. Give lifetimes on
  types descriptive names (`'session`).

Inside functions:

- Put the type on the binding instead of a turbofish (`let ids: Vec<Id> = ….collect();`),
  and annotate the result of `.into()` and `.parse()`.
- Keep iterator chains short. Bind a chain to a name before looping over it, and write a
  `for` loop instead of `Iterator::for_each`.
- Build strings with `format!` or `write!`. Templates loaded from owner TOML at run time
  are the exception.
- Reach for a function, generic, or trait before `macro_rules!`. Do not add proc macros.
- In tests, give a bare `assert!(condition)` a message that says what should have held.
  `assert_eq!` already prints both sides.

Comments and docs:

- Document every public item in `src/` with `///`, including when it panics. Use `//!`
  for module and crate docs.
- A comment states what the code cannot: a reason, an invariant, or a constraint. Delete
  comments that restate the code.

Not adopted: the guide's Chromium, Fuchsia, and Android rules (no `async`, no standard
threads, flat single-directory crates, one abstraction per crate, their mutex types, test
frameworks, and import macro) and its unstable rustfmt options.

## Adding a capability

### Model provider

Implement `backend::model::Model`. A built-in provider also supplies a
`ProviderDefinition`, is exported from `src/backend/model/mod.rs`, and is added
once to the provider list in `src/backend/model/provider.rs`. Setup and model
pickers consume that manifest; do not add a provider-specific TUI menu.

Test request mapping, streamed events, usage, errors, and compaction in the
provider module. Library consumers can register custom implementations directly
with `ModelRouter`.

### Middleware

Implement `middleware::Middleware` in one vertical module. Register tools in
`register`, lifecycle behavior in the relevant hook, and UI metadata in
`frontend` or `render`. Handle declared commands in `command`.

Only middleware shipped by `mobius-gateway` needs one registry entry in
`crates/mobius-gateway/src/harness/mobius/middleware_manifest.rs` and one construction
branch in `crates/mobius-gateway/src/harness/mobius/assembly.rs`. Do not edit the
agent loop or any frontend for capability-specific behavior.

Approval-required tools are always handled by the sandbox.
Only one messages-handling middleware may be installed.

### Tool

Implement `middleware::tools::Tool`, including its provider-facing schema,
execution mode, approval requirement, and argument validation. Register it
through `Tools::new` or the owning middleware's `register` hook. Do not add
tool-name dispatch to the agent loop or TUI.

### Frontend contribution or frontend

Commands, references, widgets, and rendered transcript blocks use the records in
`protocol` and are declared by the owning capability. A remote frontend uses the
gateway wire records carrying those types; an embedded library frontend submits
`Op` values, consumes `Event` values, and reads `Agent::frontend()` contributions.
Both decide how semantic slots, formats, and tones look.

CLI-only lifecycle and composer behavior belongs in
`crates/mobius-cli/src/frontend/catalog.rs`. Extend `protocol` only when an
existing operation, event, or contribution cannot express a frontend-neutral
need.

### Sandbox or checkpoint backend

Implement `SandboxBackend` or `CheckpointStore` and inject it through
`AgentConfig`. Wrap sandbox backends with `Sandbox`, which owns approval policy
and frontend contributions. Keep platform enforcement inside the sandbox backend
and storage layout inside the checkpoint backend. Do not put backend switches in
the loop.

## Checks

Rust 1.100.0 or newer is required. The repository pins Rust 1.100.0 with Clippy and
rustfmt in `rust-toolchain.toml`. The shared `.cargo/config.toml` enables the
experimental eight-worker compiler frontend through `RUSTC_BOOTSTRAP`, including
for the macOS workspace. Cargo's parallel crate jobs are separate. Ensure Cargo and
rustc both resolve to the pinned toolchain; unproxied system binaries can bypass
Rustup. Before handing off a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo test --workspace --doc --all-features --locked
sh .github/rules.sh
python3 .github/sync-gateway-contract.py --check
```

`.github/rules.sh` checks the rules in this file that Clippy cannot express (no clones,
no hosted-service names, no serde aliases, provider and middleware names only in their
owners). Pull requests and manually dispatched qualification run all of the above, check
the minimum toolchain with
`RUSTC_BOOTSTRAP=-1 CARGO_ENCODED_RUSTFLAGS="" cargo +1.100.0 check --workspace --all-targets --all-features --locked`
so published crates retain stable support, and lint the generated manual pages.
Relevant main pushes run formatting, rules and contract
checks only. Before tagging, manually dispatch `ci.yml` and `desktop-ci.yml` on
the exact main commit; release workflows require both applicable full qualifications.

Shortcuts: `just test` runs the test command, `just complexity` runs the Clippy command
(`cognitive_complexity` is one of its lints), `just rules` runs `rules.sh`, and `just fmt`
formats in place. Simplify touched code before handoff; do not suppress warnings to make
a change pass. Audit findings should identify the owning seam and the smallest
behavior-preserving fix.

Linux sandbox tests require Bubblewrap.

## Releases

`mobius`, `mobius-gateway`, `mobius-cli` and the macOS app `mobius-desktop` share one
version number. The three crates are published separately, each from its own tag, in
that order because each depends on the previous published crate. The macOS app is not
on crates.io: its `mobius-desktop-v<version>` tag builds, notarizes and publishes a
prerelease (`desktop-release.yml`), and `desktop-promote.yml` moves a verified release to
the update feed. Bump `build-number` in `apps/macos/Cargo.toml` with every app release.
Release automation belongs in `.github/workflows/`; do not mix it into capability
code.

Preserve `LICENSE` and `NOTICE` in crate and binary packages.

## Contribution review contract

This contract takes precedence over older repository design and review guidance.
Identify and resolve conflicting guidance instead of silently following old rules.

- Implement each behavior once. Search real callers before editing; consolidate
  duplicate or substantially similar functions (roughly 60% similarity is a review
  trigger) behind one shared API owned by the relevant capability. Generalize the
  shared behavior without erasing different validation, errors, or domain meaning.
- Apply the installed `rust-skills` skill: all 265 rules across its 26 categories.
  Read its `SKILL.md` and applicable rule files; explain any inapplicable rule or
  conflict rather than claiming blanket compliance from compilation or Clippy.
- For Rust and Swift, prioritize CPU, GPU, memory, disk I/O, and network efficiency.
  Review hot paths, allocations/copies, rendering, persistence, concurrency, and
  round trips. Measure relevant workloads; do not invent performance gains.
- Do not defer a necessary broad refactor because its immediate saving is small or
  its implementation cost is high. Even a byte or CPU cycle can justify repairing
  the architecture now; preserve correctness, security, and accessibility, and
  substantiate the improvement. Review-only requests do not authorize refactoring.
- Treat implicit coupling, layer-first folders, duplication, representable illegal
  states, god files, shotgun surgery, layering violations, pass-through wrappers,
  and validation/errors away from the input boundary as review findings. Organize
  by capability; keep one authoritative owner and validate external input there.
- Keep the agent loop simple and minimize model/tool/network round trips. Count
  calls and loop iterations for representative tasks; preserve required approvals,
  validation, cancellation, and error handling.
- Aim to reduce test count and test code while retaining coverage of meaningful
  behavior. Consolidate redundant tests; retain tests that catch real failures.
  Report before/after counts, lines, and available coverage evidence. Never remove
  useful coverage merely to meet a deletion target.

### UI and UX

- Use centrally owned tokens and components for animations, corner radii, fonts,
  font sizes, colors, shapes, dimensions, placement, transparency, shadows, and
  spacing between text, objects, and buttons, including button sizes and styles.
- iOS buttons use icons only, with accessible names; macOS buttons preferably use
  icons with text. Preserve accessible targets, keyboard use, and platform behavior.
- Treat decorative tags, italicized UI words, Instrument Serif, negative kerning,
  excessive detail, dividers, multiple color shades/opacities, bordered containers,
  diagrams (especially detailed ones), code breadcrumbs, and section numbering as
  low-quality UI indicators. Remove unnecessary decoration and centralize the
  remaining visual rules. This concerns product UI, not required code semantics or
  user-authored content. Mark UI evidence N/A for changes without a visual surface.

### Commit and pull request evidence

Every commit message and PR description must include this checklist. Check an item
only when evidence is supplied; otherwise state N/A with a reason or explicitly
record what remains unverified. Never fabricate numbers, coverage, screenshots, or
GIFs. Do not create commits or PRs unless the user requests them.

- [ ] Performance: before/after CPU, GPU, memory, disk I/O, and network measurements,
      with workload, environment, source revisions, method, and units; explain N/A.
- [ ] Agent loop: before/after model/tool calls, loop iterations, and network round
      trips for the same task; explain every required extra call or mark N/A.
- [ ] Tests: before/after test counts and test-code lines, redundant tests removed
      or consolidated, checks run, and evidence meaningful coverage is retained.
- [ ] UI/UX: shared tokens/components, animation, typography, color, shape, layout,
      transparency, shadow, spacing, button size/style, and platform conformance;
      attach before/after screenshots or GIFs for visual/interaction changes.
