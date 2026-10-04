# möbius Gateway

`mobius-gateway` is the headless möbius runtime. One process owns machine
credentials, usage, durable Bot profiles, Bot routines, and chats
with a configurable resident-session limit (32 by default). Every conversation belongs
to exactly one Bot. Project chats own their canonical workspace and transcript; each Bot also
has one persistent conversation without a project workspace. The
Bot owns its model, reasoning, capabilities, approval policy, extensions, and
prompt. The terminal, iPhone, and iPad clients can independently open different
conversations or subscribe to the same one.
Bots store enabled optional middleware IDs and generic scalar settings. The gateway advertises
the ordered middleware catalog plus integer and select control schemas, so terminal and iOS
clients render new middleware and settings without capability-specific code. New Bots enable
attachments, artifacts, context offloading, compaction, scratchpad, and subagents by default;
tasks, workspace instructions, and extensions start disabled. Context offloading masks successful
tool output after a 50,000-token trailing window. Project chats install sandboxing and workspace
tools; project-free chats expose file and image reads under the same sandbox policy, including
skill guides, while commands and file mutations remain unavailable. All chats use turn steering
and durable sessions. Only the canonical Bot conversation
installs the mandatory Persistent Chat middleware and its routine, subscription and internal
event tools. Routine runs, forks and subagents cannot inherit that middleware.
The shared `message_chat` tool sends queued or steering messages, interrupts a selected turn,
or creates a new project chat when given a workspace and its first task. `list_chats` includes
the active turn ID for interruption. Saved session hooks use the same message/interrupt protocol.
New gateways use Full Access as their Bot-creation default; saved policies remain explicit.

Compaction exposes an Automatic or Handoff policy in the same settings UI. Automatic uses the
provider compaction endpoint when available and otherwise summarizes. Handoff exposes
`write_handoff` and `new_context`: the model saves a working checkpoint and continues in the
same chat with a fresh context. The existing 250,000-token threshold requests that handoff,
clamped to leave room on smaller models. Bounded recovery restricts tools near the limit and
stops safely if the model cannot complete the transition; an explicit later turn can retry.
Original messages and tool results remain available through `search_history` and `read_history`.

Policy choices advertise incompatible capabilities in their schemas. Selecting Handoff turns
off context offloading, and the UI and gateway prevent enabling both. Tasks remain independently
optional and restore their durable list across every compaction style. Shared scratchpad notes
remain separate from the chat-local handoff checkpoint.

Install the gateway with Homebrew, or install `mobius-cli` for both commands:

```sh
brew tap citizenhicks/mobius
brew trust citizenhicks/mobius
brew install mobius-gateway
# Both commands: brew install mobius-cli
```

Cargo remains available with `cargo install --locked mobius-cli`.

`mobius-gateway reset-bot-defaults` stops the gateway and reapplies the shipped Bot-creation
template while preserving providers, credentials, installed extensions, Bots, conversations, and
workspaces. Start the gateway again after the command completes.

The separately versioned `mobius-gateway` crate is the runtime library used by those binaries.

Enabling **Computer control** on a Bot downloads its pinned Node, Playwright, and
Chromium runtime once. This works with Cargo-installed binaries and the native app.
The runtime is stored beside gateway state, normally in
`~/.mobius/gateway-runtimes/computer-control`, outside the credential directory.
Setup finishes before the Bot is saved; failed downloads can be retried by enabling
the capability again. Linux hosts need Chromium's system libraries and Bubblewrap.
`MOBIUS_COMPUTER_RUNTIME` can select an administrator-provided runtime instead.
System executables, offline resources, proxy/CA and launch policy are configurable;
see the [complete configuration reference](CONFIGURATION.md).

While the gateway is stopped, `set-desktop --enabled true` enables gateway-owned
headed Chromium on Linux with one persistent profile. Linux needs TigerVNC,
xauth and setpriv (util-linux); Openbox, tint2, feh, xterm and pcmanfm are optional
configurable applications. The gateway serves stock noVNC to the Mac and
iOS apps over a dedicated authenticated encrypted connection. There is no GUI
Office installation. The desktop has a logo wallpaper, status bar and app rail;
closing Chrome leaves Terminal and Files available. Restricted Bots with network access are refused
while the shared browser is enabled; Full access and no-network policies remain
available. Takeover cancels and drains all gateway sandbox execution before
enabling input. Worker resets retain the browser, and their first evaluation
observes without executing code. Portable backups omit the desktop profile.
Passwords and verification codes work; passkeys and Touch ID forwarding do not.

On macOS the gateway coordinates the existing app's CEF tab over a scoped Unix
DevTools broker. The app renders the chat's assigned tab; it does not launch a
separate native Chrome window. One **Bot's computer** action opens the selected
browser or remote desktop surface.

On macOS 26+, the native desktop app adds voice and native computer control.
It shares the gateway's chats and workspaces and leaves background tasks running
when closed. The desktop app is maintained in the separate private `mobius-app`
repository. Native control uses the same authenticated connection and requires
**Allow Mac control**, macOS Accessibility and Screen Recording access, and the
Bot's **Full access** policy.

Library hosts should signal shutdown through `GatewayServer::serve_until` and
await its return. The server closes connections, finishes routine dispatch, and
shuts down resident sessions, including active routines.
Dropping the serving future is not a graceful-shutdown boundary.

Initialize and pair the default gateway:

```sh
mobius-gateway init
```

**Quick Connect** is selected by default. It starts an account-free Cloudflare
Quick Tunnel, captures its temporary `trycloudflare.com` address, and displays
both the public `wss://` endpoint and local `tcp://127.0.0.1:8741` endpoint with
one ten-minute, one-use pairing code. No Cloudflare account, domain, route, or
connector token is required. The address changes whenever the gateway restarts,
so use the advanced stable-hostname option for a durable endpoint. Once a client
pairs through either endpoint, `connect` returns and the gateway keeps running in
the background. Run `mobius-gateway connect` later to advertise a fresh code while
the gateway remains running.

Every WebSocket connection requires the `mobius-noise-v1` subprotocol and an inner
`Noise_NK_25519_ChaChaPoly_SHA256` channel. Pairing codes and device tokens contain
the gateway public-key pin (`m1.<public-key>.<secret>`); the full credential is
sent only after that pinned handshake succeeds. Pairing, commands, files, events,
and authentication are encrypted between the client and gateway, including through
a TLS-terminating relay. The relay still sees upgrade credentials and traffic metadata.
The gateway's owner-only private identity file is a sibling of its authentication
file with the `.channel-key` extension (`auth.channel-key` by default); keep it
with gateway state. Existing `auth.json` credentials and revocations remain intact,
but older WebSocket clients and credentials must be updated and paired again.
Direct loopback TCP and direct TLS keep their existing transport.

For that advanced option, enter the intended hostname and connector token.
möbius starts the connector first and waits for pairing; you can then publish the
hostname to `http://127.0.0.1:8741` in Cloudflare without the missing-route
failure aborting setup. möbius stores the token in an owner-only file outside
`gateway.toml` and starts `cloudflared` with `--token-file`. The
GitHub binary archives include a pinned `cloudflared` sidecar; source and
`cargo install` builds require `cloudflared` beside `mobius-gateway` or on
`PATH`. The gateway also prints a copyable `mobius-pair:v1` setup code containing
only the public endpoint and short-lived möbius pairing code. It prefills the
Apple pairing form for confirmation and never contains the Cloudflare token.

If the selected state directory already exists, interactive initialization asks
for explicit confirmation before stopping the old gateway and deleting its
configuration, chats, providers, and paired devices.

For non-interactive setup, keep the token in an owner-only file and use:

```sh
mobius-gateway init \
  --cloudflare-hostname mobius.example.com \
  --cloudflare-token-file /private/path/tunnel-token
mobius-gateway connect
```

Plaintext listeners and clients are restricted to loopback. An iPhone, iPad,
or another machine therefore needs a routable TLS endpoint with a
publicly trusted certificate whose hostname matches that endpoint:

```sh
mobius-gateway init --listen 0.0.0.0:8741 \
  --tls-cert /absolute/path/fullchain.pem \
  --tls-key /absolute/path/private-key.pem
mobius-gateway connect --endpoint tls://gateway.example:8741
```

On iPhone, iPad, or Mac, choose **Add gateway** and enter the displayed
**Gateway address** and **One-time code**. On another terminal client, run the
displayed `mobius pair` command. Pairing consumes the code and returns a unique
bearer token; Apple clients keep it in Keychain and `mobius` keeps it in its
owner-only gateway account file. Later connections use that token, not the
one-time code.

To add another device while the gateway is already running, an authenticated
Apple client can open **Gateway → Pair another device → Create one-time code**;
an authenticated terminal client can run `/pair`. `mobius-gateway connect` is
the host-side recovery flow for a stopped gateway.

By default, owner-only state is stored under `~/.mobius/gateway`. Set
`MOBIUS_GATEWAY_STATE_DIR` or pass `--state-dir` to use another location.
Bot profiles and routine history share `bots.sqlite3`; session checkpoints and
journals use a separate database. Unsupported configuration and database versions
are rejected, not migrated or reset automatically. Back up state before upgrading.
On Linux, run the gateway account without permitted or ambient capabilities;
Bubblewrap rejects a non-root caller that retains them. Hosts that allow user,
PID, mount, and network namespaces but forbid mounting procfs inside a child PID
namespace can set `execution.procfs_mode = "empty"` in `gateway.toml`. This keeps PID isolation
and mounts an empty `/proc`; the default `private` mode mounts a private procfs.
Provider credential APIs are write-only and never return stored secret values.
Full-access file tools and shell commands can use the host filesystem; shell commands also
receive network access. They can access gateway state, TLS credentials, stored provider
credentials, and any other files or services available to the gateway account.
The configured-model catalog and Bot-creation template live in gateway configuration.
The first configured model becomes the template default. A Bot copies that template when it is
created and remains the authoritative owner of its runtime recipe. A conversation checkpoint stores
only its workspace and Bot identity, so reopening any of that Bot's conversations uses the current
Bot profile without coupling unrelated Bots or workspaces.

The gateway also owns the extension catalog. Clients may install a standalone
Agent Skill or OpenAI plugin from a credential-free HTTPS Git source. Packages
are stored as content-addressed snapshots and remain inactive until selected for
the Bot-creation template or a Bot. Executable plugin hooks require explicit review for
the installed package digest. Update and uninstall require deactivation first;
per-workspace plugin data under `.mobius/extensions` is retained.

Automation may register OpenRouter with a direct credential read from bounded
standard input, keeping the key out of arguments and URLs:

```sh
printf %s "$OPENROUTER_API_KEY" | mobius-gateway register-provider \
  --provider openrouter --model MODEL \
  --reasoning-efforts medium,none,low,high,xhigh,max \
  --web-search live --credential-stdin
```

A trusted OpenRouter-compatible connector can instead remain credentialless:

```sh
mobius-gateway register-provider --provider openrouter --model MODEL \
  --reasoning-efforts medium,none,low,high,xhigh,max \
  --web-search live \
  --base-url https://connector.example/v1 --credentialless
```

The command authenticates over the running loopback gateway, is idempotent, and
prints `{"provider":"openrouter"}` on success. Credentialless mode is rejected
for the direct OpenRouter endpoint and for providers that do not advertise it.
A provider catalog change is rejected when it would invalidate a Bot profile or
the Bot-creation template. Register the replacement under a new instance, move
affected Bots and defaults explicitly, then remove the old instance.

On macOS or Linux, open the live dashboard or gracefully stop the configured
gateway from another terminal:

```sh
mobius-gateway
mobius-gateway provider
mobius-gateway exit
```

The no-command form starts the gateway in the background when needed, then
shows every paired device and chat with active entries first, plus configured
providers, editable defaults, and usage. Use Tab plus the arrow, page, or mouse
wheel controls to scroll device and chat history. In Devices, press `u` or
Delete and confirm to unpair the selected device; the dashboard cannot unpair
its own credential. Press `p` for provider setup, `d` for defaults, or `q` to
leave without stopping the gateway. `provider` opens the same provider setup
directly. These views use this machine's saved gateway pairing.

Exit verifies the gateway's locked process record before sending
SIGINT and waits up to five seconds for shutdown.
`serve --background` starts a detached process on macOS or Linux and returns
only after the serving loop finishes initialization and publishes its locked process record. Foreground `serve`
continues to run until interrupted. Use `serve --background` for ordinary
restarts after at least one client is paired.

Gateway startup reuses a running release at the same or a newer version. When
the starting gateway executable is newer, it gracefully stops the old process
and starts a fresh one with the existing configuration, chats, and pairing state.
CLI autostart and the dashboard use this same version check.

Authenticated clients may send `set_notifications` with a request ID and a `disabled` array
containing `sessions` and/or `bots` to suppress unsolicited catalog updates. An empty array
restores them. Preferences last for that connection; initial and recovery `ready` snapshots,
explicit responses, approvals, and session events remain mandatory. Provider command clients
use these exclusions while awaiting their responses.

`ready` and `gateway_configured` carry `revisions`, a content hash of each `config`, `bots`, and
`sessions` section. Each connection receives only the sections it does not already hold: those
equal to what it last received, or to the `known` revisions and `skip` list of the optional
`catalog` hint in its `authenticate`, arrive empty and are named in `omitted`, so a client keeps
its own copy of them. A Bot catalog broadcast the connection already holds is not repeated, and
unsolicited session catalog updates arrive as `sessions_changed`: the whole ordered catalog,
unchanged chats as bare IDs, with the new `revision`. `select_session` selects a chat for file,
history, and workspace requests without replay or live events.
`get_git_diff_totals` answers `git_diff_totals` with Git's own `--numstat` line counts
(untracked files included for the unstaged scope), not the size-limited display diff.

Full session request queues reject new external work with `server_busy` before accepting it;
clients may retry that rejection with backoff. Internal shutdown, accounting, and routine
delivery still wait for capacity. Each frame write has a 30-second deadline; failed or cancelled
writes invalidate the client writer, and server write failures close the connection. Reconnect
using the last received sequence; do not blindly resubmit a mutation after losing its response.
Immutable agent-event frames share their validated JSON bytes across replay and subscribers.
The replay budget charges both event data and cached bytes; an event that exceeds that budget
is delivered live and remains recoverable from the durable journal.

If every client token is lost, stop the gateway and run the supervised pairing
flow again; existing paired clients remain valid:

```sh
mobius-gateway exit
mobius-gateway connect # add --endpoint tls://HOST:PORT for TLS
```

Each Bot may own routines with one-time, interval, or standard five-field cron
schedules, optionally bounded by an end time and pinned to a workspace. Every invocation creates a
fresh hidden conversation owned by that Bot, exposed through routine history rather than the chat
catalog; it never installs a system crontab entry or spawns a child CLI. Routine instructions are
owner-only under the gateway state directory. With no clients
and no active routines, the gateway exits after the configured idle interval (72 hours
by default). `mobius-gateway set-runtime --idle-exit-seconds SECONDS` changes that interval.
Stopping it manually also stops routine work; cron occurrences are not replayed after
restart, and intervals catch up at most one overdue occurrence.

Choose one Bot in New Chat. Each chat has one Bot, one Agent checkpoint, and one
workspace. See [Bots and context](BOTS.md) for history recovery, routine ownership,
context boundaries, scratchpad knowledge, task lists, and subagents.

### Storage and telemetry

Self-hosted gateways do not enforce a Cloud plan allowance. A configured telemetry
collector owns upload admission; `telemetry add --upload-admission` enables this
check on one POST collector. Before accepting upload bytes, the gateway reports the
requested size and a fresh storage snapshot, then relays its decision. Collector
failures reject only that upload with a retryable error. Screenshots and artifacts
bypass admission. This is a logical allowance: concurrent uploads and generated
files can exceed it, and the filesystem still has its own capacity.

`set-runtime --storage-limit-bytes BYTES` sets an informational content allowance
for reports; `--clear-storage-limit` removes it. Connected apps can inspect
storage usage and selectively purge uploaded, generated, or screenshot files while
keeping chat history.

Paired storage requests share one bounded measurement and reuse its result for up
to five seconds. Upload commits, purge, workspace edits and catalog changes
invalidate that result. Telemetry snapshots and upload admission measure fresh usage.

Outbound telemetry is disabled until a destination is configured. Operators can use
`mobius-gateway telemetry add --id ID --url HTTPS_URL --sections activity,storage`
and `mobius-gateway telemetry list` or `telemetry remove --id ID`. An endpoint can
request sections (`activity`, `usage`, `runs`, `storage`) and committed hook-event
kinds. The gateway sends the chosen sections plus envelope metadata; a hosted gateway
may have a destination configured by its host. Bearer credentials are read from an
environment variable or owner-only file and never returned in the endpoint report.
