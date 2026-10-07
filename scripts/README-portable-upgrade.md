# Offline portable-compaction upgrade

These explicitly invoked tools migrate gateway config **26 / 27 → 28**, Bot state
**7 → 8**, and checkpoint JSON **18 → 19**, plus checkpoint SQLite schema
**11 / 12 → 13**. Already-converted logical version-19 checkpoints in schema 11 are
also supported: their context is split without another compaction rewrite.
Schema-12 databases retain their already split context. Bot SQLite schema stays
at 6. The gateway itself remains strict: it does not migrate
or fall back to old formats. Nothing runs automatically during install or startup.

Use Python **3.11+** and this matching source checkout, including its owner TOML
manifests. First stop the gateway and every process writing its state, including
supervisors that could restart it. `--confirm-stopped` is your assertion, not a
process detector or distributed lock. Test a disposable copy before production.

Dry-run is the default and never creates backups or modifies input files:

```sh
python3 scripts/upgrade-portable-config.py \
  --gateway-config /absolute/state/gateway.toml \
  --bots-db /absolute/state/bots.sqlite3
python3 scripts/upgrade-portable-compaction.py \
  --database /absolute/state/checkpoints.sqlite3
```

Supply the real explicit paths; there is no state discovery. Repeat `--database`
for each checkpoint database. Preflight **all** intended files before applying.
Unknown versions, targeted settings, checkpoint fields or SQLite schema layouts
are rejected. Noncanonical TOML formatting may be rejected without writes.
For the config **27 → 28** reasoning-catalog change alone, supply only
`--gateway-config` to the config tool; Bot and checkpoint formats do not change.

To apply, repeat each command with:

```text
--apply --confirm-stopped --backup-dir /absolute/private/upgrade-backup
```

Use a **different directory for each tool**. The config tool requires a new backup
directory; the checkpoint tool accepts an existing private directory (0700).
The parent of each backup directory must already exist. Both retain verified originals with private file permissions (0600), synchronize
backups before writing, and report counts/hashes without printing contents or
credentials. Config/Bot changes preserve revisions and unrelated settings;
checkpoint changes preserve transcript/execution journals and logical event contents, sequences,
receipts, queued messages, pending approvals/tools, metadata and cumulative usage.

The tools are idempotent: repeat dry-runs must report zero format and storage-tuning changes. Format updates to each
SQLite database run in one transaction. **There is no atomic transaction across
TOML and multiple databases.** An interruption can leave some resources upgraded.
Keep the gateway stopped after any error. Retain the verified backups and inspect
which resources were applied; either finish the same upgrade or restore the whole
original set before using the old binary. Do not mix old/new formats or restore a
SQLite file while stale WAL/SHM sidecars or writers remain. No automatic rollback
is attempted after an uncertain commit or directory-sync failure.

After all dry-runs report no changes, use the new gateway's `check-config` command
for the selected state directory, then test loading a chat, queued-message
recovery, model selection and a fork on the disposable copy before rollout.
`check-config` remains the full validation authority, including provider-manifest
rules. The script does not maintain a separate list of preset or custom providers.
It reads the matching source's provider manifests and model catalogs. For editable
providers with an owner catalog, a `models/<provider>.toml` file beside the supplied
gateway config takes precedence. Keep that directory with disposable config copies.
Malformed catalogs needed for seeding stop conversion; the tool does not silently
fall back to a different catalog. Catalog source files and overrides are read only.

## Semantic changes and loss

- Compaction `mode = "automatic"` becomes `allow_model_compaction = "off"`;
  `"handoff"` becomes `"on"`. Retired native/offloading settings are removed.
  Missing or unknown selections are refused rather than guessed.
- Config 27 `model_ids` and provider-wide `reasoning_efforts` become ordered
  full `models` records containing `id`, `label`, `description`, `context_window`,
  `reasoning`, `default_reasoning` when applicable, and `tool_discovery`.
  Reasoning choices contain `id`, `label`, and an optional `description`.
  Each custom model receives the old effort list in its original order, with its
  first effort as the explicit default. Empty lists have no default. Matching
  owner presets retain their model and reasoning display metadata; unknown IDs
  use the ID as their label and an empty description. An unknown model's context
  window comes from the provider's declared default model, or the gateway default
  if none is declared; its tool discovery comes from the provider manifest.
  Existing Anthropic,
  Kimi and DeepSeek setups with empty legacy lists receive their owner catalog's
  full model records and defaults. OpenAI Codex and OpenAI Socket retain
  locked catalogs with `models = []`; OpenRouter and Responses have no seed catalog.
  Config 26 also receives the compaction conversion before advancing to 28.
  Current config 28 is unchanged: full metadata and reasoning arrays are required,
  nonempty reasoning lists require a default from that list, and empty lists must
  have no default. Optional provider `tool_discovery` overrides are preserved.
  Duplicate IDs/efforts, unknown entry fields, old provider-wide fields and the
  unreleased intermediate map/compact model formats are refused.
  Catalogs retain the runtime limits of 64 entries, 1,024 UTF-8 bytes per entry,
  and 16 KiB of entry text. Model context windows must be positive; model and
  reasoning labels must be nonblank and at most 1,024 bytes, with descriptions
  at most 16 KiB. Nonempty model catalogs also enforce valid selected
  models/efforts, unambiguous routes, and 64 total model routes across the gateway.
- Active encrypted `compaction` / `compaction_summary` items are removed. Their
  meaning **cannot be recovered by this script**. Original journals and backups
  remain intact; an old fork may need its parent's history to recover detail.
  Ordinary encrypted reasoning and existing offloaded placeholders are preserved.
- Version-18 checkpoints receive `context_model_route` from their saved model route. A
  nonempty context with no known route is refused. Existing plaintext handoff
  notes are retained; valid old saved handoff notes are restored when no projection
  exists. Rewritten active context advances its epoch once and clears obsolete
  last-usage/rewrite/cache markers; cumulative usage stays unchanged.
- SQLite schemas 12 and 13 store active context in ordered `context_items` rows. The
  latest checkpoint JSON becomes a header with **no** `context` key; session epoch
  and item count must match the rows. Schema changes, header updates and ordered
  row insertion commit together in one transaction. Existing version-19 logical
  context/owner/epoch stay unchanged. Empty databases also advance to schema 13.
  Existing valid split-context databases need no context rewrite; mixed inline
  and split context, retired epochs, missing rows and noncontiguous indices are
  rejected. Transcript journals remain unchanged. Event storage conversion is described below.

## Historical event storage

Schema 13 wraps original events in private storage envelopes. Large all-text tool
outputs (strictly more than 4,096 UTF-8 bytes) can instead reference their exact
immutable transcript item. The script indexes each session's transcript once,
matching call ID, error flag and every ordered text part. Ambiguous matches,
small/mixed-media outputs, unknown event fields and unmatched events remain
inline with the original event JSON retained. Repeated call IDs alone never
establish a match. Public event payloads remain unchanged when read by the gateway.

Dry-run reports `event_envelopes_written`, `tool_outputs_deduplicated` and the net
`event_json_bytes_removed` (which can be negative when wrapping only small events).
These are logical JSON bytes, not guaranteed immediate file-size savings. Context
splitting, event conversion and schema-version changes commit together. Original
backups retain the old event representation; schema-13 validation refuses missing
or mismatched transcript references. The tool does not backfill additional event
references on repeated schema-13 runs.

## Explicit disk tuning

The checkpoint tool also enables SQLite `auto_vacuum=INCREMENTAL`, including for
already-upgraded schema-13 databases. Dry-run reports `from_auto_vacuum`,
`storage_tuning_changed`, `from_freelist_count` and `vacuum_required`. Applying an OFF → INCREMENTAL
conversion performs a full `VACUUM` after the format transaction commits. It can
require substantial temporary disk space and time; it is never run at gateway
startup. FULL → INCREMENTAL needs no rebuild. When a format upgrade or mode change is
applied to an already vacuum-capable database, the tool exhausts one incremental
vacuum operation to reclaim its free pages, including pages freed by event
deduplication. Already-current schema-13 INCREMENTAL databases with no free pages remain a no-op.
Remaining free pages keep explicit apply eligible, so interrupted reclamation can
be retried even when the schema and vacuum mode already changed.
New gateway databases enable the
mode at creation; the gateway applies its 64 MiB WAL journal size limit on its own
connections. This limit controls retained journal allocation after reset, not a
hard bound on an active WAL or a promise to reclaim every free database page.

`VACUUM` cannot run inside the format transaction. If it fails, schema 13 may
already be committed while disk tuning remains incomplete. The tool retains the
verified original backup, reports its path, and attempts a failure receipt with
`storage_tuning_complete=false`; keep the gateway stopped, inspect the receipt
and available disk space, and rerun. It never automatically restores old files.
Logical table contents and schema version are checked before and after tuning;
auto-vacuum settings are included separately in the preflight fingerprint. Once
schema 13 and INCREMENTAL are present and no free pages remain, repeated apply
runs do nothing.

Run only synthetic self-checks without accessing a gateway:

```sh
python3 scripts/upgrade-portable-config.py --self-test
python3 scripts/upgrade-portable-compaction.py --self-test
```
