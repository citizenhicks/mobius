<p align="center">
  <img src="https://raw.githubusercontent.com/citizenhicks/mobius/main/mobius-app/apple/Sources/MobiusApp/Assets.xcassets/MobiusLogo.imageset/MobiusLogo.svg" width="160" height="160" alt="möbius">
</p>

# möbius

möbius is a small, frontend-neutral Rust framework for coding agents. Its shipped
runtime has one headless composition root: `mobius-gateway`. The terminal and
Apple apps are thin clients of that gateway; they do not own agent behavior.

## Architecture

```text
Terminal client ─┐
SwiftUI client ──┼── versioned gateway protocol ⇄ mobius-gateway ──> Agent
Other clients ───┘                                     │
                                                        ├─ model router
                                                        ├─ sandbox
                                                        ├─ checkpoints
                                                        └─ middleware stack
```

### Core and agent

The [`mobius`](https://crates.io/crates/mobius) crate is the embeddable core.
[`src/agent/`](src/agent/) owns one durable session and its linear command,
model, approval, and tool loop. [`src/protocol/`](src/protocol/) defines the
frontend-neutral operations, events, and presentation records around that loop.
A library caller supplies an `AgentConfig`; the core does not select a frontend
or hide runtime dependencies behind global state.

### Middleware

[`src/middleware/`](src/middleware/) contains optional capabilities. One
middleware owns its tools, state, prompt section, runtime hooks, commands,
widgets, references, event rendering, and tests. `MiddlewareStack` validates one
ordered list, and that declaration order is observable.

The core exposes one typed hook suite directly on `Middleware`:

| Hook | Purpose |
| --- | --- |
| `session_start` (`SessionStart`) | Start or resume a main agent or subagent; after compaction, re-establish hidden context. Startup failures unwind completed starts in reverse order. |
| `user_prompt_submit` (`UserPromptSubmit`) | Inspect, enrich, or reject a prompt before it enters durable context. |
| `pre_model` (`PreModel`) | Apply durable context changes before a primary model step. |
| `model_request` (`ModelRequest`) | Add request-only material after every `pre_model` hook has finished. |
| `pre_tool_use` (`PreToolUse`) | Inspect, enrich, rewrite, or deny each normalized tool call before it is persisted or authorized. |
| `permission_request` (`PermissionRequest`) | Allow, deny, or defer a sandbox request immediately before the user would be asked. |
| `post_tool_use` (`PostToolUse`) | Change feedback or add context after a real tool execution, without pretending to undo its side effects. |
| `pre_compact` (`PreCompact`) | Inspect the exact context and optionally stop the turn before compaction. |
| `post_compact` (`PostCompact`) | Inspect the committed compacted context and optionally stop before `session_start(compact)`. |
| `stop` (`Stop`) | Finish normally or request one bounded continuation. The context identifies whether the agent is main or subagent. |
| `turn_end` (`TurnEnd`) | Clean up transient turn state after either completion or abort. |
| `session_end` (`SessionEnd`) | Release session resources in reverse declaration order. |

All hooks run in declaration order except `session_end`. `pre_model` and
`model_request` are two complete stack passes, so no request-only decoration can
precede a later durable rewrite. Policy decisions are typed outcomes; hook
errors remain infrastructure failures. Static prompt sections are composed once
when the agent is created, and changing its Bot profile recreates the prompt and
tool catalog without adding either to conversation history.

`continue: false` is a typed core decision at session-start and compaction
boundaries: it stops a recovered or compacting turn, while an idle startup has
no turn to stop. Every middleware can use that lifecycle control.

The Extensions middleware loads standalone Agent Skills and explicitly activated
OpenAI plugin snapshots. A plugin is one package rooted at
`.codex-plugin/plugin.json`; its declared skills are exposed as
`plugin-name:skill-name`, and its command hooks adapt the OpenAI event contract
onto the typed lifecycle above. Hooks run synchronously in this slice. MCP and
app contributions are not yet supported.

The gateway installs skills and plugins from credential-free HTTPS Git sources
as content-addressed, read-only snapshots. Installation is inactive. An extension
may be selected independently for the Bot-creation template or one Bot; its skills
load immediately, while executable hooks stay disabled until their complete
package digest is reviewed. Selections follow an installed extension across
updates, and changed executable code requires a new review. Selected extensions
must be removed from every Bot profile before they can be uninstalled.
User-level Agent/Codex skill roots and project-local `.agents/skills` and
`.codex/skills` remain discovered, read-only inputs outside the managed catalog.

### Providers

[`src/backend/model/`](src/backend/model/) owns the `Model` transport contract,
`ModelRouter`, and built-in `ProviderDefinition` manifests. A manifest supplies
generic setup metadata, authentication, model choices, and advertised
capabilities. Each adapter converts its private wire format into `ModelEvent`
and `ModelOutput` before the agent loop sees it, so setup screens and model
pickers do not branch on provider IDs.

### Backends

[`src/backend/checkpoint/`](src/backend/checkpoint/) defines `CheckpointStore`
and owns durable checkpoints, event journals, transcript pages, and the session
catalog. [`src/backend/sandbox/`](src/backend/sandbox/) defines
`SandboxBackend`; `Sandbox` wraps an injected backend with approval policy,
background-process ownership, and frontend contributions.

Protected local modes use Seatbelt on macOS and Bubblewrap on Linux and fail
closed when the selected platform sandbox is unavailable. Filesystem confinement
stays active under every approval policy except **Full access**. Full-access
shell commands can reach anything available to the gateway account, including
gateway state and credentials; file tools remain workspace-scoped.

### Gateway

[`mobius-gateway`](https://crates.io/crates/mobius-gateway) is the only shipped
owner of an `Agent`. It explicitly assembles one agent per active conversation
from its owning Bot and owns authentication, paired clients, workspaces,
artifacts, Git, usage, Bot profiles and routines, manual Bot swarms, and the
extension catalog. Its versioned wire protocol translates authenticated client
requests into core operations and publishes core events plus capability
contributions.

### CLI

[`mobius-cli`](https://crates.io/crates/mobius-cli) provides the `mobius`
Ratatui client and the `mobius-gateway` executable. The terminal frontend sends
gateway operations and renders gateway events and the capability catalog;
[`crates/mobius-cli/src/frontend/`](crates/mobius-cli/src/frontend/) owns only
terminal lifecycle, input, and presentation.

### SwiftUI app

[`mobius-app/apple/`](mobius-app/apple/) is one SwiftUI gateway client for iPhone
and iPad. It uses the same versioned protocol and capability contributions as the
CLI, keeps paired client credentials in Keychain, and owns Apple lifecycle,
storage, navigation, and rendering—not agent, provider, or middleware behavior.

## Install

Download `mobius-<version>-<target>.tar.gz` and its checksum from
[GitHub Releases](https://github.com/citizenhicks/mobius/releases). The archive
contains `mobius`, `mobius-gateway`, and `cloudflared`. Rust users can install the
two möbius commands with Rust 1.98 or newer:

```sh
cargo install --locked mobius-cli
```

Cargo does not install `cloudflared`; Quick Connect requires it beside
`mobius-gateway` or on `PATH`.

Users upgrading from the earlier split packages should run
`cargo install --force --locked mobius-cli` once so Cargo transfers both commands to the CLI
package.

Then run `mobius` from the workspace it should own:

```sh
cd /path/to/repository
mobius
```

On first use, the CLI initializes the machine-wide gateway with a loopback listener
and Cloudflare Quick Tunnel, provisions its local credential, starts the gateway in
the background, and opens `/login` when no provider is configured. The first
configured model becomes the Bot-creation template default. Each CLI invocation
selects an existing Bot and creates an independent conversation for its current
directory; other terminal and app frontends can connect to the same gateway and
open separate or shared conversations. The conversation owns its workspace and
transcript. Its Bot owns the model, reasoning, capabilities, approval policy,
extensions, and prompt. The core `mobius` crate is linked into the binaries and
is not a separate runtime prerequisite.

Plaintext remains limited to loopback. Run `mobius-gateway connect` to advertise
both that local TCP endpoint and the Quick Tunnel's public WSS endpoint with one
single-use pairing code; pairing through either exchanges it for a per-client
token used on later connections. A direct TLS listener remains available as an advanced
alternative. See the
[gateway guide](https://github.com/citizenhicks/mobius/blob/main/crates/mobius-gateway/README.md),
the [CLI guide](https://github.com/citizenhicks/mobius/blob/main/crates/mobius-cli/README.md),
and the [Apple guide](https://github.com/citizenhicks/mobius/blob/main/mobius-app/apple/README.md)
for manual and remote setup.

To run the Rust binaries from this checkout:

```sh
cargo build -p mobius-cli
cargo run -p mobius-cli --bin mobius
```

## Embed the core

möbius requires Rust 1.98 or newer.

```toml
[dependencies]
mobius = "0.9"
```

The caller owns composition:

```rust,no_run
use std::path::Path;
use std::sync::Arc;

use mobius::Result;
use mobius::agent::{Agent, AgentConfig, create_agent};
use mobius::backend::checkpoint::{CheckpointStore, sqlite::SqliteCheckpoint};
use mobius::backend::model::{Model, ModelRouter, openai::OpenAi};
use mobius::backend::sandbox::{ApprovalPolicy, Sandbox, local::LocalSandbox};
use mobius::middleware::{Middleware, MiddlewareStack};
use mobius::middleware::tools::Tools;

async fn build_agent(
    workspace: &Path,
    api_key: String,
    model_id: &str,
) -> Result<Agent> {
    let model: Arc<dyn Model> = Arc::new(OpenAi::new(
        api_key,
        "https://api.openai.com/v1",
        model_id,
    )?);
    let models = Arc::new(ModelRouter::new("default", model));
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(workspace)?),
        ApprovalPolicy::Ask,
    ));
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(workspace.join("mobius.sqlite3"))?);
    let middleware: Vec<Arc<dyn Middleware>> = vec![Arc::new(Tools::coding())];

    create_agent(AgentConfig::new(
        models,
        sandbox,
        checkpoints,
        MiddlewareStack::new(middleware)?,
        "You are a concise coding agent.",
    ))
    .await
}
```

Frontends submit
[`protocol::Op`](https://docs.rs/mobius/latest/mobius/protocol/enum.Op.html) values and consume
events from `Agent`. Framework capabilities may also contribute frontend-neutral commands,
references, widgets, and rendered blocks. A frontend decides how those contributions look;
capability implementations do not depend on terminal code. Interrupts target a specific turn, and
events carry an optional submission ID so command-driven and unsolicited system events remain
distinct.

## Contributing

Read [AGENTS.md](https://github.com/citizenhicks/mobius/blob/main/AGENTS.md) before changing the framework. It defines module ownership,
capability extension points, required checks, and the no-compatibility rule while
the public contract remains under active development.

Release tags are intentionally separate:

- `mobius-vX.Y.Z` publishes the framework crate and creates its GitHub Release.
- `mobius-gateway-vX.Y.Z` publishes the gateway crate and attaches server binaries.
- `mobius-cli-vX.Y.Z` publishes the CLI crate and attaches downloadable binaries to a GitHub
  Release.

Publish `mobius`, then `mobius-gateway`, then `mobius-cli`, waiting for each dependency to appear in
the crates.io index. Creating a tag is a release action; ordinary pushes and pull requests only
run CI. The release workflow expects a `CARGO_REGISTRY_TOKEN` repository secret for
crates.io publication.

## License

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for third-party
attributions.
