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
Serde defaults: existing version-26 files may omit them or specify only changed fields.
This addition does not bump configuration version or protocol 90. Unknown fields still
fail, so typos cannot silently select defaults. Structurally incompatible versions are
rejected without rewriting files.

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
rendering share the output budget. Compaction exposes `keep_recent_tokens`,
`native_retained_tokens`, `reserve_tokens`, `handoff_reserve_divisor`,
`handoff_warning_reserves` and `handoff_urgent_reserves`. The host's byte/token estimate
is shared by compaction and offloading; measured provider usage remains authoritative.
Advertised subagent controls and absent-setting defaults respect the host ceilings,
which a paired client cannot raise. Explicit saved values above a lowered ceiling
are rejected for the operator to resolve.

## Model transport and provider data

`[model_transport]` applies to built-in model routes and provider authentication started
by the gateway. Durations use milliseconds:

| Field | Default |
| --- | ---: |
| `http_connect_timeout_ms` | 10000 |
| `http_idle_timeout_ms` | 180000 |
| `socket_connect_timeout_ms` | 15000 |
| `socket_io_timeout_ms` | 5000 |
| `socket_idle_timeout_ms` | 300000 |
| `stream_retry_limit` | 5 |
| `stream_retry_backoff_ms` | 200 |
| `stream_retry_max_backoff_ms` | 3200 |
| `compaction_retry_limit` | 2 |
| `compaction_retry_backoff_ms` | 200 |
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
realtime endpoints/voices and tariffs. Middleware prompts, labels and default policies
live beside their owners. Distributors can edit those source defaults without changing
execution logic; runtime policy uses the fields above. No global provider registry is added.
DeepSeek's time-dependent pricing table includes an optional holiday calendar. When a
potential peak window lacks verified calendar coverage, cost remains unknown rather
than selecting a guessed tariff. Custom DeepSeek roots also report unknown pricing.

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
Supervisors own restart, uptime holds and volume policy.
Existing files that explicitly set the removed `runtime.hold_socket` field must drop
that field before validation; uptime holds belong to the host supervisor.

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
listeners. Without the requirement/variable, a local gateway runs independently of an
account or subscription. `storage_limit_bytes` is informational and separate from local
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
