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
| `crates/mobius-gateway/` | Headless composition, auth, Bots, sessions, routines, Swarms, artifacts, and usage |
| `crates/mobius-cli/src/frontend/` | Thin terminal gateway client and rendering |

`mobius-gateway` is the only shipped owner of an `Agent`. The CLI sends gateway
operations and renders gateway events; capability behavior remains in its framework
module.

## Design rules

- Reuse an existing trait or protocol record before adding a new abstraction.
- Keep dependencies explicit. Registries assemble objects; they are not service locators.
- Middleware declaration order is observable hook and prompt-section order;
  `session_end` alone unwinds in reverse.
- `prompt_section` is composed once at agent creation; use `pre_model` for dynamic durable state and `model_request` for request-only decoration.
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
- Add the smallest behavior-focused test that would catch the change.
- Do not add compatibility code. This project has no legacy contract: no old
  names, aliases, dual reads or writes, fallback state discovery, or migrations.

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

Only middleware shipped by `mobius-gateway` needs one composition setting and one
construction branch in `crates/mobius-gateway/src/assembly.rs`. Do not edit the
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

Rust 1.98 or newer is required. Before handing off a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --locked --no-deps
cargo test --workspace --all-targets --all-features --locked
```

Linux sandbox tests require Bubblewrap. `just test` and `just fmt` are shortcuts
for local iteration.

## Releases

`mobius`, `mobius-gateway`, and `mobius-cli` have separate versions and releases.
Publish them in that order because each depends on the previous published crate.
Release automation belongs in `.github/workflows/`; do not mix it into capability
code.

Preserve `LICENSE` and `NOTICE` in crate and binary packages.
