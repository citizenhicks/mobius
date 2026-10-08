# Gateway configuration

The gateway is a standalone host for the open-source framework. Host policy belongs
to the local operator; Bots own their model and middleware settings. The built-in
`mobius` Bot identity and its deletion protection stay fixed for safety.

## Configuration file

`gateway.toml` lives in the state directory (`~/.mobius/gateway` by default).
`--state-dir PATH` takes precedence over `MOBIUS_GATEWAY_STATE_DIR`. Retain owner-only
permissions for configuration and secrets. Edit host policy while stopped, validate,
then restart:

```sh
mobius-gateway print-default-config
mobius-gateway --state-dir /srv/agent/state exit
mobius-gateway --state-dir /srv/agent/state check-config
mobius-gateway --state-dir /srv/agent/state serve
```

`print-default-config` prints complete default TOML without creating state or exposing
credentials. Optional paths are omitted until configured. New operator sections use
Serde defaults allow omitted operator sections to use their documented defaults.
Unknown fields fail, so typos cannot silently select defaults. Configuration version
28 stores complete model records per editable provider setup. Upgrade version 26 or 27 offline
with the [portable upgrade tool](../../scripts/README-portable-upgrade.md) before
starting this gateway; incompatible files are rejected without rewriting them.

## Execution and capacity

```toml
[execution]
command_timeout_seconds = 900
shell_executable = "/usr/bin/bash"
procfs_mode = "private"
bytes_per_token = 3.0
subagent_max_depth = 32
subagent_max_concurrency = 128
subagent_max_agents = 512

[connections]
authenticated = 96
pre_authentication = 16
authentication_timeout_seconds = 30
active_sessions = 64
pending_uploads = 16

[auth]
paired_clients = 64
pairing_lifetime_seconds = 1200
```

Execution defaults are a 120-second command deadline, the framework's Bash-compatible
shell, 4 bytes/token and subagent ceilings 16/64/256. A configured shell must accept
`-c` scripts; isolated execution does not require Bash-specific startup flags. `bubblewrap_executable` accepts a
trusted absolute executable on Linux. Isolation flags, credential removal and private
state masking remain safety boundaries. Additional environment names cannot override
credential denial. Shell/isolation executables must exist outside agent-writable roots.

On Linux, `execution.procfs_mode` accepts `private` (default) or `empty`. The framework
keeps private user and PID namespaces for isolated commands in both modes. `private` mounts procfs inside
that PID namespace; hosts that deny nested procfs mounts receive an actionable error.
`empty` mounts an empty directory at `/proc` and must be selected explicitly only when
the host already provides PID isolation. Network isolation, protected state masking
and private temporary directories remain enforced. The framework never switches modes
automatically. Library hosts select the same typed `ProcfsMode` through `LocalSandbox`.

Capacity defaults are 32 authenticated connections, 8 pre-authentication connections,
32 resident sessions, 8 pending uploads per connection and 32 paired clients including
the local operator. Counts accept 1–4096. The authentication deadline defaults to
5 seconds (1–3600) and covers TCP, TLS/Noise and the authentication frame together.
Pairing lifetime defaults to 600 seconds (1–86400). Lowering the client limit preserves
existing identities and blocks new pairing beyond that limit.

Bot middleware settings expose `sandbox.tool_output_bytes` (40000; maximum 1048576)
and `sandbox.background_commands` (4; maximum 256). Command capture and final tool
rendering share the output budget. Compaction exposes `at_tokens` (250000),
`reserve_tokens` (16384), and `allow_model_compaction` (`off` or `on`, default `off`).
Automatic compaction saves a plaintext checkpoint and resets context when the
token threshold or destination model budget requires it. Enabling
`allow_model_compaction` also lets the model request an early reset. Original
messages and tool results remain available in searchable history. The host's
byte/token estimate guides compaction; measured provider usage remains authoritative.
Advertised subagent controls and absent-setting defaults respect the host ceilings,
which a paired client cannot raise. Explicit saved values above a lowered ceiling
are rejected for the operator to resolve.

## Model transport and provider data

`[model_transport]` applies to built-in model routes and provider authentication started
by the gateway. Durations use milliseconds. `max_request_bytes` limits the complete
serialized outgoing model request, including images, text and tool schemas:

| Field | Default |
| --- | ---: |
| `max_request_bytes` | 25165824 (24 MiB) |
| `http_connect_timeout_ms` | 10000 |
| `http_idle_timeout_ms` | 180000 |
| `socket_connect_timeout_ms` | 15000 |
| `socket_io_timeout_ms` | 5000 |
| `socket_idle_timeout_ms` | 300000 |
| `stream_retry_limit` | 5 |
| `stream_retry_backoff_ms` | 200 |
| `stream_retry_max_backoff_ms` | 3200 |
| `voice_start_timeout_ms` | 30000 |
| `voice_io_timeout_ms` | 5000 |
| `voice_call_timeout_ms` | 3600000 |
| `oauth_request_timeout_ms` | 30000 |
| `oauth_callback_timeout_ms` | 900000 |
| `oauth_callback_request_timeout_ms` | 5000 |
| `device_code_timeout_ms` | 900000 |

Base URLs are configurable per provider instance, including native Codex routes. Custom
roots apply consistently to the route's HTTP, socket, image and voice operations without
falling back to official model endpoints. OAuth issuer and account-usage endpoints remain
separate provider-owned data. Endpoint-scoped credentials must be configured explicitly;
default-host credentials are not silently reused for a proxy. A browser-authenticated
custom endpoint must first be registered by the local operator. Paired clients may
reuse that registered endpoint, but cannot introduce or retarget it.

Typed embedded TOMLs beside `src/backend/model/` adapters own provider labels,
descriptions, credential environment names, wire headers, search modes, OAuth parameters,
realtime endpoints/voices and model catalogs. Middleware prompts, labels and default policies
live beside their owners. Distributors can edit those source defaults without changing
execution logic; runtime policy uses the fields above. No global provider registry is added.

Model catalogs (`anthropic.toml`, `openai.toml`, `kimi.toml`, `deepseek.toml`) can be
replaced without a rebuild: a file with the same name in `<state dir>/models/` is read at
gateway start instead of the bundled copy. A file that fails to parse or names an
unlisted default model or reasoning effort is ignored with a warning. For editable
providers, these files supply initialization defaults; edit an existing setup's
stored model records to change that setup.

### Provider model configuration

Every provider uses the same setup and registration path. Only Codex and OpenAI
socket lock their model catalogs; their setup `models` arrays remain empty.
Anthropic, Kimi and DeepSeek seed a new setup from their bundled catalogs.
OpenRouter and Local start empty. After saving, an editable setup's `models` array
is authoritative: changing the bundled catalog does not add removed models back.

Each stored model has the same full record as a bundled model. Operators may edit
all its fields in `gateway.toml` while stopped. For example:

```toml
[[configured_providers.work.models]]
id = "model-a"
label = "Model A"
description = "Custom endpoint model"
context_window = 128000
tool_discovery = "rebuild"
default_reasoning = "high"
reasoning = [
  { id = "low", label = "Low", description = "" },
  { id = "high", label = "High", description = "" },
]
```

A nonempty reasoning list requires a default from that list. Plain models use
`reasoning = []` and omit `default_reasoning`. Explicit Bot reasoning choices must
belong to the selected model. Editable setups together are limited to 64
model/effort routes. Context windows must be positive.

The setup's `selection.base_url`, `selection.web_search`, and optional
`selection.tool_discovery` apply to its models. Discovery accepts `native` or
`rebuild`; omission uses each model's default and any provider endpoint policy.
The gateway advertises supported modes and rejects unsupported choices before
saving. Kimi supports `rebuild` only. Choosing `native` for a custom endpoint
requires that endpoint to implement the provider's native discovery protocol.

App registration edits model IDs, reasoning choices/defaults and provider-level
URL, web search and discovery. It preserves stored labels, descriptions, context
windows and reasoning labels for matching IDs. These metadata fields remain
operator-controlled. New IDs inherit matching catalog metadata, or the provider's
default context window and discovery mode when absent from its catalog.

### Command-line setup

`register-provider` uses the same registration path as apps. Repeated `--model-id`
flags replace the model list while preserving retained models' metadata and
reasoning choices. Omit them to preserve the live list. Set reasoning only with
`--models-json`: omit a model's `reasoning_efforts` to retain its choices, or supply
an empty array to clear them. For example:

```sh
mobius-gateway --state-dir /srv/agent/state register-provider \
  --provider openrouter --instance work --model model-a \
  --base-url https://openrouter.ai/api/v1 --web-search off \
  --tool-discovery rebuild \
  --models-json '[{"id":"model-a","reasoning_efforts":["low","high"],"default_reasoning":"high","context_window":128000,"label":"Model A"},{"id":"model-b","reasoning_efforts":[]}]'
```

`--models-json` conflicts with `--model-id`. Its optional
`label`, `description` and `context_window` fields require the local operator
credential; ordinary paired clients cannot overwrite them. Omitted metadata stays
unchanged. Empty or omitted model lists preserve existing setups and seed new
catalog-backed setups. Omitted setup label, tint and image model IDs retain the
live gateway values; new setups use the provider label, default tint and empty
image list. An explicit wire `image_model_ids: []` clears that list. The command
reads disk configuration only to locate the gateway, never to fill omitted setup
metadata. Omit `--tool-discovery` to preserve an existing override.
Credentials continue through the command's existing credential input options.

Automation refreshes use `--preserve-selection` to retain live model and image
catalogs, labels, tint, selected model, reasoning effort and web search while updating
credentials or endpoint settings. Supplied model/image lists only seed a new setup;
ordinary registration without this flag applies intentional catalog edits.
`--if-configured` skips registration of a missing optional setup. Both decisions
are made against live gateway state under the mutation lock. Cloud uses this same
command for all providers; it does not rebuild catalogs or call a separate wire
registration script.

### Independent text, image and live catalogs

Text, image generation and live voice are configured independently. An empty or
omitted `selection.model` with `models = []` disables text for that setup, including
its text model choices in the apps. Codex and OpenAI socket text catalogs remain
locked when text is enabled. A Bot still needs a configured text route, but its
image and live routes may use separate provider setups and credentials.

Optional `image_models` and `voice_models` arrays override the corresponding
built-in presets. Omission retains those presets; `[]` disables that capability.
Each preset has `id`, `label`, `description` and `variants`; variants have `id`,
`label` and `description` and represent image quality or live voices. Live models
require at least one voice. Nonempty `image_model_ids` replace the built-in image
presets and cannot be combined with `image_models`. App label/key edits preserve
the operator's media overrides.

For example, this setup exposes live voice only:

```toml
[configured_providers.live]
label = "Live"
tint = "blue"
models = []
image_models = []

[configured_providers.live.selection]
instance = "live"
provider = "openai_socket"
model = ""
web_search = "off"

[[configured_providers.live.voice_models]]
id = "gpt-live-1"
label = "Live"
description = ""
variants = [{ id = "cedar", label = "Cedar", description = "" }]
```

Supply the setup credential through the existing credential registration path.
Edit these stored presets while stopped and restart the gateway to apply them.

## Computer resources

`[computer]` separates the resource directory from executable locations:

| Field | Meaning/default |
| --- | --- |
| `mode` | `managed` installs dependencies; `preinstalled` never downloads |
| `directory` | Resources; omitted derives a private managed cache beside state |
| `node_executable`, `npm_executable` | Optional system executable paths |
| `playwright_module` | Optional external Playwright package directory |
| `browsers_directory` | Optional browser cache directory |
| `install_timeout_seconds` | Complete installation deadline, 600 |
| `download_connect_timeout_seconds` | Download connection deadline, 30 |
| `download_timeout_seconds` | Distribution request deadline, 180 |
| `node_download_base_url` | HTTPS mirror, `https://nodejs.org/dist` |
| `tar_executable` | Command/absolute executable, `tar` |
| `ca_file` | Additional PEM CA bundle; otherwise respects `SSL_CERT_FILE` |

Export matching worker/docs/package resources without downloading dependencies:

```sh
mobius-gateway export-computer-resources --directory /opt/agent/computer
```

```toml
[computer]
mode = "preinstalled"
directory = "/opt/agent/computer"
node_executable = "/usr/bin/node"
playwright_module = "/opt/agent/node_modules/playwright"

[computer.browser]
executable = "/usr/bin/chromium"
profile_directory = "/srv/agent/private-browser"
sandbox = true
arguments = ["--no-first-run", "--no-default-browser-check"]
window_size = [1440, 900]
window_position = [40, 40]
viewport = [1440, 900]
start_page = "about:blank"
```

System Node, external Playwright and system Chromium can be combined without bundled
Node/browsers. Mismatched workers fail with instructions to re-export current resources.
Installer subprocesses inherit proxy, download mirror and CA settings, not provider
credentials. Their executable search path contains the selected Node directory plus
`/usr/bin` and `/bin`; it does not inherit the gateway's ambient `PATH`. PEM CA configuration also applies to Node archive downloads; `SSL_CERT_FILE`
is mapped to Node's extra CA setting.

Browser sandboxing defaults to enabled. A root container must explicitly choose
`computer.browser.sandbox = false` if its environment requires that policy. Root with
sandboxing enabled produces a clear configuration error before browser startup. Private
profiles are protected from normal workspace commands. CDP remains loopback-only, and
operator arguments cannot replace worker-owned profile/debugging invariants. Headless
and headed launch paths share the browser policy.

## Linux desktop and branding

`set-desktop --enabled true` enables the desktop. `[computer.desktop]` accepts
`resolution` (default `[1365,768]`), `depth` (24), `display_start`/`display_end` (100–200)
and `startup_timeout_seconds` (15). `[computer.desktop.tools]` selects `xauth`, `xvnc`,
`vncconfig` and `setpriv` by command/absolute path. Linux requires Bubblewrap, Chromium
system libraries, TigerVNC, xauth and **setpriv (util-linux)**.

Application sections `window_manager`, `panel`, `wallpaper`, `terminal`, `files` accept
`enabled`, `executable` and `arguments`. Defaults Openbox/tint2/feh/xterm/pcmanfm are
optional: missing applications are skipped. `[computer.desktop.branding]` accepts paths
for `wallpaper`, `logo`, `browser_icon`, `terminal_icon`, `files_icon`, `panel_config`,
`status_config`, and text `browser_label`, `terminal_label`, `files_label`. Omitted assets
use bundled defaults. No hosting-provider theme or lifecycle API is required.

## Lifecycle, storage and telemetry

`[runtime]` owns `idle_exit_seconds` (259200; 0 disables), optional `ingress`, optional
`storage_limit_bytes`, `require_access_lease` (false), `access_grace_seconds` (0).
Supervisors own restart and volume policy. An optional telemetry activity hook can
hold the host awake while the gateway has work.
Existing files that explicitly set the removed `runtime.hold_socket` field must drop
that field before validation; no hosting-provider hold path is built into the gateway.

```sh
mobius-gateway set-runtime --idle-exit-seconds 0
```

Set lease policy in `gateway.toml` while stopped:

```toml
[runtime]
require_access_lease = true
access_grace_seconds = 30
```

An explicit required lease fails startup without a valid Unix-seconds
`MOBIUS_GATEWAY_ACCESS_EXPIRES_AT`; expiry plus configured grace closes connections and
listeners. The operator supplies the expiry; without the requirement or the variable,
the gateway runs with no access expiry. `storage_limit_bytes` is informational and separate from local
filesystem capacity. Its existing minimum is 64 MiB when supplied. A local gateway
may use any available space. Reported quota usage counts content blobs and files in
registered chat workspaces. Nested workspaces are charged once; separate file copies,
including materialized attachments, count separately. Gateway databases, browser
profiles, and other runtime state are measured but excluded from the file allowance.
An incomplete workspace measurement makes upload admission fail closed.
Only an explicitly
configured telemetry collector can reject user uploads; screenshots/artifacts bypass
admission. General symlinks count their metadata without following targets. Symlinks in
charged blob storage make that measurement incomplete and admission fails closed.

Telemetry starts with no collectors. Paired clients can inspect safe reports/request
delivery, but cannot rewrite destinations or select host credential environment names.
Mutation requires the reserved authenticated local operator identity over a local
connection, provisioned by the gateway CLI. Labels/client kind grant no privilege.

```sh
mobius-gateway telemetry add --id collector \
  --url https://collector.example/ingest --sections activity,storage \
  --bearer-file collector-token
mobius-gateway telemetry list
```

Relative bearer files resolve against gateway state and require owner-only permissions;
`--bearer-env MOBIUS_COLLECTOR_TOKEN` is another explicit operator source. Collector
environment names must start with `MOBIUS_`; provider and gateway credential names cannot be selected. Reports show collector origins
and delivery status; bearer sources, URL paths/queries and custom header values are
omitted. `--upload-admission` enables one POST decision collector, and failures reject
that upload with a retryable error. No collector means no remote allowance enforcement.

HTTPS and loopback HTTP are accepted by default. Private HTTP requires local operator
policy, changed while stopped:

```toml
[telemetry.policy]
allow_insecure_http = true
request_timeout_seconds = 20
upload_admission_timeout_seconds = 30
```

After validation, an operator can add a private collector with `telemetry add`.

`[telemetry.policy]` contains `allow_insecure_http` (false), `request_timeout_seconds`
and `upload_admission_timeout_seconds` (both 10, range 1–3600). Admission's deadline
includes its fresh measurement. Each sink owns its interval, snapshot sections, hook
events and safe envelope fields. Revisions prevent stale configuration updates.

### Resource measurements (0.16.20)

Add `resources` to an operator-configured POST collector's `sections` to collect
local hardware measurements on macOS and Linux. For workload correlation, request
`activity,resources,runs`; `runs` already reports completed model/tool call counts
and failures. Those counts exclude unfinished turns and are not transport retry or
OS syscall counts. Gateway uptime and instance identity remain in every envelope.

```sh
mobius-gateway telemetry add --id resources \
  --url https://collector.example/ingest \
  --sections activity,resources,runs --every-seconds 30 \
  --bearer-file collector-token
```

`resources` has `version`, `status`, `measured_at_ms`, `sample_interval_ms`, and:

| Scope | Measurements |
| --- | --- |
| `host` | CPU percent (0–100 across the CPUs visible to the OS), logical CPUs, total/available memory, total/used swap, host uptime, mounted-disk read/write byte totals. |
| `gateway` | Current gateway process CPU percent (100 per occupied core), accumulated CPU milliseconds, resident memory, process disk read/write byte totals. Does not include child processes. |
| `container` | Linux cgroup v2 CPU microseconds, throttled microseconds/periods, current memory bytes, local CPU quota in cores and memory limit, per-device I/O byte and operation totals. Includes descendants of the current cgroup. |
| `gpu` | Local GPU utilization and available memory counters. macOS uses unprivileged IORegistry accelerator properties; Linux tries `/usr/bin/nvidia-smi`, then DRM/sysfs counters (including AMD where exposed). No provider GPU information is available. |

Memory and I/O use bytes. CPU percentages need two sufficiently separated samples;
first and too-close samples report `null`. Counter reset boundaries are gateway
process start, host boot, device replacement, or cgroup recreation, according to
scope. Too-close samples reuse gateway process counters until the OS CPU sampling
window has elapsed (currently 200 ms on macOS and Linux), preserving their baseline.
Collectors must discard negative deltas and use measurement times for rates.
Disk counters describe OS-reported storage I/O, not every application read/write:
caches can satisfy operations without disk I/O. Disk rows refer to mounted devices;
do not sum overlapping logical volumes as physical-device throughput.

Container limits are **local** limits: ancestor quotas and CPU affinity may constrain
the process further. `local_cpu_quota_unlimited` and
`local_memory_limit_unlimited` distinguish the cgroup's `max` setting from an
unreadable limit. Container memory includes accounting such as page cache and is
not interchangeable with gateway resident memory. Host values describe what the
OS exposes, which may exceed a container's allocation.

Unsupported/unreadable optional counters are `null` or have `status = "unavailable"`;
non-Linux container measurements have `status = "unsupported"`. Linux GPU discovery
can report `not_detected`; this does not assert that inaccessible hardware is absent.
Apple unified-memory GPU usage is `system_memory_used_bytes`, not dedicated VRAM.
Driver-specific counters are best effort and may be absent after an OS/driver update.

Collection starts only when a requested snapshot includes `resources`. Sinks due in
the same tick share the measurement. OS collection runs off the async executor and
outside the session lock; at most one OS worker remains outstanding. OS and GPU
collection each have a two-second response deadline. A timed-out OS call cannot be
forcibly cancelled, so subsequent requests report busy until it returns. GPU command
probes are killed on cancellation/timeout and their output is capped at 256 KiB.
DRM/sysfs reads share the guarded OS worker. Device
lists are bounded to 64 rows. A resource failure is reported in the section and does
not fail the other snapshot sections. Stop envelopes retain their existing minimal
shape without a final hardware probe. No telemetry collector is enabled by default.

An optional local activity hook uses the same runtime activity as telemetry snapshots,
independently of collector configuration or delivery success:

```toml
[telemetry.activity_hook]
command = ["/usr/bin/systemd-inhibit", "--what=idle:sleep", "--mode=block", "/bin/cat"]
idle_grace_seconds = 5
timeout_seconds = 5
retry_seconds = 5
```

On macOS the same contract can use `command = ["/usr/bin/caffeinate", "-i", "/bin/cat"]`.

The gateway starts one foreground command while authenticated native clients, running
or queued session work, approvals, background commands, subagents, pending deliveries
or due routines need execution. Dashboard connections and future schedules alone do
not hold the host awake. Runtime changes wake measurement promptly; failed measurements
retain the hold. Idle grace accepts 0–3600 seconds, cleanup/measurement timeout 1–60,
and retry interval 1–3600. Omit the section to disable the hook.

The command is an argument vector, with an absolute executable outside gateway state
and every agent workspace. It runs from `/` with a cleared environment and
`PATH=/usr/bin:/bin`, without inherited provider credentials. Any script arguments,
configuration and credentials must also be operator-owned outside writable workspaces.
The foreground process must acquire its own inhibitor, retain it until stdin closes,
then release it and exit. Closing stdin also handles an abruptly killed gateway.
Normal idle/shutdown closes stdin, bounds cleanup, and terminates remaining process-group
members. Unexpected exits are logged and retried while activity requires a hold.
The hook is configured in `gateway.toml` while stopped; paired clients cannot alter it.

Hosts that suspend the entire VM must arrange an external wakeup for future routines;
an internal gateway timer cannot run while the VM is suspended. Hook commands must not
detach, reset another holder's inhibitor, or depend on telemetry requests for cleanup.

## Extension sources

HTTPS Git URLs accept custom ports. Explicit SSH URLs such as
`ssh://git@git.example.com:2222/team/tools.git` use native keys, host-key verification,
operator SSH configuration and `SSH_AUTH_SOCK` in batch mode. Embedded passwords,
HTTPS usernames, queries, fragments and unsafe refs are rejected. Git credential helpers
and repository hooks remain disabled during installation. Extension hook execution
retains its explicit authorization contract.

## Environment reference

| Variable | Meaning |
| --- | --- |
| `MOBIUS_GATEWAY_STATE_DIR` | State root; CLI flag wins |
| `MOBIUS_GATEWAY_ENDPOINT` | Client endpoint override, including TLS hostname |
| `MOBIUS_GATEWAY_TOKEN` | Explicit client credential |
| `MOBIUS_GATEWAY_TOKEN_FILE` | Saved client-token file location |
| `MOBIUS_COMPUTER_RUNTIME` | Pre-provisioned resources when `computer.directory` is omitted |
| `MOBIUS_GATEWAY_ACCESS_EXPIRES_AT` | Explicit access expiry, Unix seconds |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Default-endpoint provider credentials |
| `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`, `OPENROUTER_API_KEY` | Other default-endpoint provider credentials |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | Proxy policy; installer and extension Git also forward lowercase variants |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | Trust settings; PEM file added to download trust |
| `NODE_EXTRA_CA_CERTS`, `NODE_USE_SYSTEM_CA`, `npm_config_cafile` | Node/npm installer CA settings |
| `PLAYWRIGHT_DOWNLOAD_HOST`, `PLAYWRIGHT_CHROMIUM_DOWNLOAD_HOST` | Browser distribution mirrors |
| `SSH_AUTH_SOCK` | Native SSH agent for extension Git |
| `HOME`, `USERPROFILE`, `PATH` | Native home/executable discovery |
| `USER`, `USERNAME` | Optional local session display name |

Bearer environment names are explicitly operator-selected. Normal commands scrub gateway,
provider and collector credentials. Full Access remains the existing explicit host-wide
trust choice. `MOBIUS_GATEWAY_VERSION` has no lifecycle meaning; the running package
version is reported from build metadata rather than inferred from a hosting environment.
