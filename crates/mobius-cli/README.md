# möbius CLI

`mobius-cli` is the reference Ratatui client for a `mobius-gateway`. The gateway owns agent
composition, providers, sessions, sandboxing, usage, and scheduled work.

## Install the client

Download one `mobius-cli` archive and checksum from
[GitHub Releases](https://github.com/citizenhicks/mobius/releases):

- Apple Silicon macOS: `aarch64-apple-darwin`
- x86_64 Linux: `x86_64-unknown-linux-gnu`

Verify with `shasum -a 256 -c FILE.sha256`, extract the included `mobius` and
`mobius-gateway` binaries into one directory, and put it on your `PATH`. Rust users and other
macOS or Linux architectures can install both commands with Rust 1.89 or newer:

```sh
cargo install --locked mobius-cli
```

If the earlier standalone gateway package is installed, run
`cargo install --force --locked mobius-cli` once to transfer both commands to this package.

## Gateway included

The CLI package installs its gateway beside `mobius`; the core `mobius` crate is linked into the
binaries. Run the CLI from the workspace for the chat you want to create:

```sh
cd /path/to/repository
mobius
```

With no explicit gateway endpoint or token, the first run initializes the machine-wide default
gateway with both a loopback listener and Cloudflare Quick Tunnel, provisions the CLI's local
credential, and starts `mobius-gateway` in the background. A later `mobius-gateway connect`
advertises the local TCP and public WSS endpoints with one pairing code that works through either
endpoint.
If no model provider is configured, the same three-page `/login` flow opens immediately. The
first configured model becomes the gateway default for new chats.
Each run creates a chat scoped to the current directory; `/workspace <gateway-path>` creates and
selects another chat without changing other running chats. For a source checkout, build both
commands from the CLI package:

```sh
cargo build -p mobius-cli
cargo run -p mobius-cli --bin mobius
```

Plaintext is restricted to loopback. A gateway reachable over the network must use a
publicly trusted TLS certificate matching its public hostname. Initialize it once, then run the
supervised connection flow while the gateway is stopped:

```sh
mobius-gateway init --listen 0.0.0.0:8741 \
  --tls-cert /absolute/path/fullchain.pem \
  --tls-key /absolute/path/private-key.pem
mobius-gateway connect --endpoint tls://gateway.example:8741
```

Then use the endpoint and one-time code it displays on the client machine:

```sh
mobius pair tls://gateway.example:8741 <one-time-code>
mobius
```

`mobius pair` saves and selects the endpoint together with the token returned by the gateway; no
environment variable is needed. A remote terminal opens an existing gateway chat, so create the
first gateway-host workspace chat from an Apple or local frontend.
If the gateway is already running, create another code with `/pair` from an authenticated terminal
or **Gateway → Pair another device** in an Apple client.

If local state already exists without a saved CLI token, stop the gateway and run the supervised
pairing flow in another terminal:

```sh
mobius-gateway exit
mobius-gateway connect
mobius pair tcp://127.0.0.1:8741 <one-time-code>
```

Run one task file without the TUI:

```sh
mobius run path/to/task.md
# From a source checkout:
cargo run -p mobius-cli --bin mobius -- run path/to/task.md
```

The selected chat workspace—not the CLI process—is the command and file boundary. An approval
prompt aborts a headless run, so scheduled work that edits files or runs commands needs an
appropriate chat approval policy.

Manage the gateway extension catalog without creating or opening a chat:

```sh
mobius extensions
```

The same lifecycle screen is available as `/extensions` from an idle chat. Installation accepts
an HTTPS Git URL or a GitHub tree URL; update, uninstall, and digest-bound hook trust operate on
the selected installed extension.

Inside the TUI, `/cron new [task]` starts the model-assisted setup when scheduling is enabled for
the chat. The model asks for missing task or frequency details, then an approval-required gateway
tool saves and registers the final task. Ordinary chat cannot create schedules. `/cron` also
exposes list, reschedule, delete, run, and history operations for the selected chat; every
scheduled execution creates a separate durable result chat.

`/login` is the single provider setup path. It opens the guided provider screen, where API keys
can be pasted into a masked field, the environment variable declared by the provider manifest is
used when the field is empty, and device-login providers show their login flow. There is no
separate environment-name setting. Setup covers the built-in manifests compiled into both the
gateway and CLI; injected `ModelRouter` entries are library-only. The final page confirms the
provider's model and reasoning choice. The gateway owns the complete configured-model catalog and
new-chat default; `/model` only changes the selected chat to one of those available routes.
`/agent` opens a one-page capability and approval-policy editor without changing the selected
provider or system prompt. Required gateway capabilities remain visible but cannot be deselected.
Secrets are sent directly to the gateway and never returned to the CLI.
`/gateway` lists saved endpoints and opens a second page to pair a new endpoint; reconnect and
delete act on the selected saved gateway. Explicit endpoint or token environment variables make
that screen read-only until they are unset.

API-key providers use their standard environment variables:

```sh
export OPENAI_API_KEY=...
export MOONSHOT_API_KEY=...
export OPENROUTER_API_KEY=...
export ANTHROPIC_API_KEY=...
```

The CLI stores only an owner-readable selected endpoint and endpoint-token map at
`~/.mobius/gateway-tokens.json`. `MOBIUS_GATEWAY_TOKEN` overrides the saved token explicitly;
`MOBIUS_GATEWAY_TOKEN_FILE` changes the account-file path.

## Terminal contributions

The TUI is a thin subscriber to the framework capability catalog:

- Capabilities own their commands, status widgets, references, and capability-specific rendering.
- `/` opens both CLI shell commands and commands contributed by framework capabilities.
- `$` references are contributed by the Extensions middleware.
- `@` workspace-file completion is available for a local plaintext gateway; TLS gateways do not
  scan similarly named paths on the client machine.

The CLI owns only shell lifecycle and presentation commands: `/help`, `/gateway`, `/extensions`,
`/agent`, `/login`, `/pair`, `/profile`, `/new`, `/clear`, `/model`, `/reasoning`, `/status`,
`/interrupt`, and `/exit`. Capabilities contribute commands such as `/artifacts` and `/cron`, so
the menu changes with the installed gateway capabilities. The gateway always contributes `/resume`
as the single saved-chat picker; it lists chats across every workspace.

The Sora-themed TUI uses the full terminal. The mouse wheel and Page Up/Page Down scroll the chat;
Ctrl-T opens a full-screen transcript view, releases mouse capture for native drag-to-copy, and
scrolls with Arrow or Page Up/Page Down. Up/Down and Ctrl-P/Ctrl-N navigate composer history.

Sandboxing runs on the gateway host and fails closed when its platform sandbox is unavailable.

## License

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for upstream attribution.
