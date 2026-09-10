# möbius Gateway

`mobius-gateway` is the headless möbius runtime. One process owns machine
credentials, usage, durable Bot profiles, Bot routines, and manual Bot swarms
while hosting up to 32 independent conversations. Every conversation belongs
to exactly one Bot and owns only its canonical workspace and transcript; the
Bot owns its model, reasoning, capabilities, approval policy, extensions, and
prompt. The terminal, iPhone, and iPad clients can independently open different
conversations or subscribe to the same one.
Bots store enabled optional middleware IDs and generic scalar settings. The gateway advertises
the ordered middleware catalog plus integer and select control schemas, so terminal and iOS
clients render new middleware and settings without capability-specific code. New Bots enable
attachments, artifacts, context offloading, compaction, scratchpad, and subagents by default;
tasks, workspace instructions, and extensions start disabled. Context offloading masks successful
tool output after a 50,000-token trailing window. The gateway always installs sandboxing,
workspace tools, turn steering, and durable sessions.

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
# Mac menu bar app: brew install --cask mobius-app
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

On macOS 26+, the optional **möbius-app.app** adds voice and native computer control in the
menu bar. It shares the gateway's chats and workspaces and leaves background tasks
running when closed. Cargo installs the command-line binaries; install the native
app separately through Homebrew or the gateway release. Native control uses the
same authenticated connection and requires **Allow Mac control**, macOS
Accessibility and Screen Recording access, and the Bot's **Full access** policy.
See the [macOS guide](https://github.com/citizenhicks/mobius/blob/main/crates/mobius-gateway/macos/README.md)
for building, packaging, and local connection requirements.

Library hosts should signal shutdown through `GatewayServer::serve_until` and
await its return. The server closes connections, finishes routine dispatch, stops
Swarm delivery, and shuts down resident sessions, including active routines.
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
namespace can set `MOBIUS_GATEWAY_SANDBOX_PROC=empty`. This keeps PID isolation
and mounts an empty `/proc`; the default `private` mode mounts a private procfs.
Provider credential APIs are write-only and never return stored secret values.
Full-access shell commands can use the host filesystem and network while file tools
remain workspace-scoped. Those shell commands can access gateway state, TLS credentials,
stored provider credentials, and any other files or services available to the gateway
account.
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
only after that process owns the gateway process record. Foreground `serve`
continues to run until interrupted. Use `serve --background` for ordinary
restarts after at least one client is paired.

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
and no active routines, the gateway exits after 72 hours. Stopping it manually also stops routine
work; cron occurrences are not replayed after restart, and intervals catch up at most one overdue
occurrence.

Swarms are optional manual groups of Bots with one appointed leader. In each Bot's
capability settings, choose `bots.collaboration = swarm` to opt in; the default is
`off`. The generic settings UI renders this choice from the Bots manifest. Disabled
Bots retain their saved membership and pending messages, but do not receive Swarm
work. Bot profiles, ordinary chats, and self-routines remain independent.

Inside Swarm Chat, an exact enabled Bot `@handle` routes work to its durable hidden
participant conversation; later messages reuse it. `@user` creates a durable
attention notification without opening or injecting a visible user chat. Routine
results stay in routine history unless the running Bot explicitly posts to the
Swarm. See [Bots and context](BOTS.md) for history recovery, compaction handoffs,
shared scratchpad knowledge, task lists, subagents, and escalation rules.
