#!/usr/bin/env python3
"""Offline, stopped-gateway checkpoint upgrade. Never modifies transcript journals."""

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import tempfile
import unittest
from unittest import mock
import uuid


class UpgradeError(ValueError):
    """Fixed validation messages that never embed input contents."""


OLD_VERSION = 18
NEW_VERSION = 19
OLD_SCHEMA = 11
NEW_SCHEMA = 13
MAX_SQLITE_INTEGER = (1 << 63) - 1
CONTEXT_TABLE = """CREATE TABLE context_items (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    item_index INTEGER NOT NULL CHECK (item_index >= 0),
    item_json TEXT NOT NULL,
    PRIMARY KEY (session_id, epoch, item_index)
) WITHOUT ROWID"""
MAX_EPOCH = (1 << 64) - 1
CACHE_FIELD = "_mobius_prompt_cache_breakpoint"


def restored_prompt():
    text = (Path(__file__).resolve().parents[1] / "src/middleware/compaction.toml").read_text()
    match = re.search(r'^prompt_restored\s*=\s*(".*")\s*$', text, re.M)
    if match is None:
        raise UpgradeError("cannot read the owning compaction restore prompt")
    return json.loads(match[1])


def has_notes_projection(context):
    if not isinstance(context, list):
        raise UpgradeError("checkpoint context is not an array")
    return any(isinstance(item, dict) and item.get("_mobius_internal") == "handoff_notes"
               for item in context)


def normalize(checkpoint, old_notes=None):
    if not isinstance(checkpoint, dict):
        raise UpgradeError("checkpoint is not an object")
    allowed = {"version", "session_id", "session_context", "metadata", "catalog_visible",
               "first_user_message", "model_route", "context_model_route", "sequence", "context",
               "delivered_once", "context_epoch", "compaction_count", "last_context_rewrite",
               "total_usage", "last_usage", "pending_messages", "active_execution",
               "active_model_step", "execution_stats", "pending_tools", "pending_approval"}
    if not set(checkpoint) <= allowed:
        raise UpgradeError("unknown checkpoint fields")
    version = checkpoint.get("version")
    if type(version) is not int or version not in (OLD_VERSION, NEW_VERSION):
        raise UpgradeError("unsupported checkpoint version")
    context = checkpoint["context"]
    if not isinstance(context, list):
        raise UpgradeError("checkpoint context is not an array")
    if version == NEW_VERSION:
        if "context_model_route" not in checkpoint:
            raise UpgradeError("version 19 checkpoint has no context owner field")
        owner = checkpoint["context_model_route"]
        if owner is not None and (not isinstance(owner, str) or not owner.strip()):
            raise UpgradeError("checkpoint context owner is invalid")
        if type(checkpoint.get("context_epoch")) is not int or not 0 <= checkpoint["context_epoch"] <= MAX_EPOCH:
            raise UpgradeError("checkpoint context epoch is invalid")
        if any(isinstance(item, dict) and item.get("type") in ("compaction", "compaction_summary") for item in context):
            raise UpgradeError("version 19 still contains retired native compaction")
        return False, 0, False
    if "context_model_route" in checkpoint:
        raise UpgradeError("version 18 checkpoint already has a context owner")
    route = checkpoint["model_route"]
    if type(checkpoint.get("context_epoch")) is not int or not 0 <= checkpoint["context_epoch"] <= MAX_EPOCH:
        raise UpgradeError("checkpoint context epoch is invalid")
    if route is not None and (not isinstance(route, str) or not route.strip()):
        raise UpgradeError("checkpoint model route is invalid")
    if route is None and context:
        raise UpgradeError("nonempty checkpoint context has no model route")
    retained = [
        item for item in context
        if not isinstance(item, dict)
        or item.get("type") not in ("compaction", "compaction_summary")
    ]
    removed = len(context) - len(retained)
    restored = old_notes is not None and not has_notes_projection(retained)
    if restored:
        if not isinstance(old_notes, str) or not old_notes.strip() or len(old_notes.encode("utf-8")) > 21_000:
            raise UpgradeError("legacy plaintext handoff notes are invalid")
        retained.append({
            "role": "user", "_mobius_internal": "handoff_notes",
            "content": [{"type": "input_text", "text": restored_prompt() + "\n\n" + old_notes}],
        })
    if route is None and retained:
        raise UpgradeError("restored checkpoint context has no model route")
    if removed or restored:
        epoch = checkpoint["context_epoch"]
        if type(epoch) is not int or not 0 <= epoch < MAX_EPOCH:
            raise UpgradeError("checkpoint context rewrite epoch cannot advance")
        checkpoint["context"] = retained
        checkpoint["context_epoch"] = epoch + 1
        checkpoint["last_usage"] = None
        checkpoint["last_context_rewrite"] = None
        for item in retained:
            if not isinstance(item, dict):
                continue
            content = item.get("content")
            if isinstance(content, list):
                for part in content:
                    if isinstance(part, dict):
                        part.pop(CACHE_FIELD, None)
    else:
        rewrite = checkpoint.get("last_context_rewrite")
        if rewrite is not None:
            reasons = rewrite["reasons"]
            rewrite["reasons"] = [reason for reason in reasons if reason != "context_offloading"]
            if not rewrite["reasons"]:
                checkpoint["last_context_rewrite"] = None
    checkpoint["context_model_route"] = route
    checkpoint["version"] = NEW_VERSION
    return True, removed, restored


def json_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise UpgradeError("duplicate checkpoint JSON key")
        result[key] = value
    return result


def reject_nonfinite(_value):
    raise UpgradeError("nonfinite checkpoint JSON")


def database_digest(connection, *, storage_settings=True):
    digest = hashlib.sha256()
    # iterdump omits user_version; a version-only change must invalidate preflight too.
    digest.update(str(connection.execute("PRAGMA user_version").fetchone()[0]).encode())
    digest.update(b"\n")
    if storage_settings:
        digest.update(str(connection.execute("PRAGMA auto_vacuum").fetchone()[0]).encode())
        digest.update(b"\n")
    for statement in connection.iterdump():
        digest.update(statement.encode("utf-8"))
        digest.update(b"\n")
    return digest.hexdigest()


def sync_file_and_parent(path):
    for target in (path, path.parent):
        descriptor = os.open(target, os.O_RDONLY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)


def candidates(connection):
    if connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
        raise UpgradeError("checkpoint integrity check failed")
    schema = connection.execute("PRAGMA user_version").fetchone()[0]
    if schema not in (OLD_SCHEMA, 12, NEW_SCHEMA):
        raise UpgradeError("expected checkpoint SQLite schema 11, 12 or 13")
    expected = {
        "sessions": "session_id parent_session_id parent_sequence latest_sequence latest_event_sequence latest_checkpoint_json session_context_json execution_stats_json catalog_visible first_user_message created_at updated_at",
        "middleware_state": "scope key value_json",
        "transcript_delta": "session_id sequence items_json created_at",
        "message_receipts": "session_id submission_id",
        "execution_journal": "session_id sequence record_json started_at_ms",
        "event_journal": "session_id sequence recorded_at_ms event_kind model_step_id stream_phase delta_bytes event_json stream_metrics_json",
    }
    if schema >= 12:
        expected["sessions"] += " context_epoch context_count"
        expected["context_items"] = "session_id epoch item_index item_json"
    tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    if tables != set(expected) or connection.execute("SELECT 1 FROM sqlite_master WHERE type IN ('trigger','view')").fetchone():
        raise UpgradeError("unsupported checkpoint database schema")
    for table, columns in expected.items():
        if {row[1] for row in connection.execute(f"PRAGMA table_info({table})")} != set(columns.split()):
            raise UpgradeError("unsupported checkpoint table schema")
    if connection.execute("PRAGMA foreign_key_check").fetchone():
        raise UpgradeError("checkpoint foreign key check failed")
    old_notes = dict(connection.execute(
        "SELECT scope, value_json FROM middleware_state WHERE key = 'compaction.handoff'"
    ))
    changes = []
    for session_id, sequence, original in connection.execute(
        "SELECT session_id, latest_sequence, latest_checkpoint_json FROM sessions ORDER BY session_id"
    ):
        checkpoint = json.loads(original, object_pairs_hook=json_object, parse_constant=reject_nonfinite)
        if not isinstance(checkpoint, dict):
            raise UpgradeError("checkpoint is not an object")
        if (checkpoint["session_id"] != session_id or type(checkpoint["sequence"]) is not int
                or not 0 <= checkpoint["sequence"] <= MAX_EPOCH or checkpoint["sequence"] != sequence):
            raise UpgradeError("checkpoint row does not match its index")
        if schema >= 12:
            validate_split_checkpoint(connection, session_id, checkpoint)
            continue
        notes = None
        if checkpoint.get("version") == OLD_VERSION and session_id in old_notes and not has_notes_projection(checkpoint["context"]):
            notes = json.loads(old_notes[session_id], object_pairs_hook=json_object,
                               parse_constant=reject_nonfinite)
            if not isinstance(notes, str):
                raise UpgradeError("legacy plaintext handoff notes are not text")
        _, removed, restored = normalize(checkpoint, notes)
        epoch = checkpoint["context_epoch"]
        if epoch > MAX_SQLITE_INTEGER:
            raise UpgradeError("checkpoint epoch exceeds SQLite INTEGER")
        items = checkpoint.pop("context")
        changes.append((session_id, original, json.dumps(
            checkpoint, ensure_ascii=False, separators=(",", ":"), allow_nan=False
        ), removed, restored, epoch, [
            json.dumps(item, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
            for item in items
        ]))
    return changes


def validate_split_checkpoint(connection, session_id, checkpoint):
    if checkpoint.get("version") != NEW_VERSION or "context" in checkpoint:
        raise UpgradeError("split context requires a version 19 header without inline context")
    epoch, count = connection.execute(
        "SELECT context_epoch, context_count FROM sessions WHERE session_id=?", (session_id,)
    ).fetchone()
    if (type(epoch) is not int or type(count) is not int or not 0 <= epoch <= MAX_SQLITE_INTEGER
            or count < 0 or checkpoint.get("context_epoch") != epoch):
        raise UpgradeError("checkpoint context header does not match session columns")
    context = []
    for row_epoch, index, raw in connection.execute(
        "SELECT epoch,item_index,item_json FROM context_items WHERE session_id=? ORDER BY epoch,item_index",
        (session_id,),
    ):
        if row_epoch != epoch or index != len(context):
            raise UpgradeError("context rows have retired epochs or noncontiguous indices")
        context.append(json.loads(raw, object_pairs_hook=json_object, parse_constant=reject_nonfinite))
    if len(context) != count:
        raise UpgradeError("checkpoint context row count does not match session columns")
    # Reconstruct only for validation; idempotent inspection never rewrites a valid header/row.
    logical = dict(checkpoint, context=context)
    normalize(logical)


def decode_json(raw):
    return json.loads(raw, object_pairs_hook=json_object, parse_constant=reject_nonfinite)


def output_key(call_id, is_error, output):
    if not isinstance(call_id, str) or type(is_error) is not bool or not isinstance(output, list):
        return None
    if not all(isinstance(part, dict) and set(part) == {"type", "text"}
               and part["type"] == "input_text" and isinstance(part["text"], str) for part in output):
        return None
    if sum(len(part["text"].encode("utf-8")) for part in output) <= 4096:
        return None
    return call_id, is_error, tuple(part["text"] for part in output)


def event_changes(connection):
    """One transcript index per session, exact payload keys; ambiguous matches stay inline."""
    current = connection.execute("PRAGMA user_version").fetchone()[0] == NEW_SCHEMA
    fields = {"type", "turn_id", "call_id", "name", "output", "is_error"}
    for (session,) in connection.execute("SELECT session_id FROM sessions ORDER BY session_id"):
        index = {}
        if not current:
            for sequence, raw in connection.execute(
                    "SELECT sequence,items_json FROM transcript_delta WHERE session_id=? ORDER BY sequence", (session,)):
                items = decode_json(raw)
                if not isinstance(items, list):
                    continue  # Preserve unrelated/custom journal payloads without references.
                for offset, item in enumerate(items):
                    if not isinstance(item, dict) or item.get("type") != "function_call_output":
                        continue
                    key = output_key(item.get("call_id"), item.get("_mobius_is_error"), item.get("output"))
                    if key is not None:
                        index[key] = None if key in index else (sequence, offset)
        for sequence, raw in connection.execute(
                "SELECT sequence,event_json FROM event_journal WHERE session_id=? ORDER BY sequence", (session,)):
            event = decode_json(raw)
            if current:
                validate_stored_event(connection, session, event)
                continue
            replacement = '{"storage":"inline","event":' + raw + '}'
            msg = event.get("msg") if isinstance(event, dict) else None
            if (isinstance(event, dict) and set(event) <= {"submission_id", "msg"}
                    and ("submission_id" not in event or isinstance(event["submission_id"], str))
                    and isinstance(msg, dict) and set(msg) == fields and msg["type"] == "tool_call_end"
                    and all(isinstance(msg[key], str) for key in ("turn_id", "call_id", "name"))):
                key = output_key(msg["call_id"], msg["is_error"], msg["output"])
                target = index.get(key) if key is not None else None
                if target is not None:
                    stored = {"storage": "tool_output", "submission_id": event.get("submission_id"),
                              **{key: msg[key] for key in ("turn_id", "call_id", "name", "is_error")},
                              "transcript_sequence": target[0], "item_index": target[1]}
                    replacement = json.dumps(stored, ensure_ascii=False, separators=(",", ":"))
            yield session, sequence, raw, replacement, replacement.startswith('{"storage":"tool_output"')


def validate_stored_event(connection, session, stored):
    if not isinstance(stored, dict):
        raise UpgradeError("stored event is not an envelope")
    if stored.get("storage") == "inline" and set(stored) == {"storage", "event"}:
        return
    fields = {"storage", "submission_id", "turn_id", "call_id", "name", "is_error", "transcript_sequence", "item_index"}
    if (set(stored) != fields or stored["storage"] != "tool_output"
            or not all(isinstance(stored[key], str) for key in ("turn_id", "call_id", "name"))
            or (stored["submission_id"] is not None and not isinstance(stored["submission_id"], str))
            or type(stored["is_error"]) is not bool
            or any(type(stored[key]) is not int or not 0 <= stored[key] <= MAX_SQLITE_INTEGER
                   for key in ("transcript_sequence", "item_index"))):
        raise UpgradeError("unsupported stored event envelope")
    row = connection.execute("SELECT items_json FROM transcript_delta WHERE session_id=? AND sequence=?",
                             (session, stored["transcript_sequence"])).fetchone()
    items = decode_json(row[0]) if row else None
    if not isinstance(items, list) or stored["item_index"] >= len(items):
        raise UpgradeError("stored tool output reference is missing")
    item = items[stored["item_index"]]
    if (not isinstance(item, dict) or item.get("type") != "function_call_output"
            or item.get("call_id") != stored["call_id"]
            or type(item.get("_mobius_is_error")) is not bool
            or item["_mobius_is_error"] != stored["is_error"]
            or output_key(item["call_id"], item["_mobius_is_error"], item.get("output")) is None):
        raise UpgradeError("stored tool output reference does not match")


def tune_storage(connection):
    before = database_digest(connection, storage_settings=False)
    mode = connection.execute("PRAGMA auto_vacuum").fetchone()[0]
    connection.execute("PRAGMA auto_vacuum=INCREMENTAL")
    if mode == 0:
        connection.execute("VACUUM")
    else:
        # SQLite can yield one row per reclaimed page; exhaust the statement.
        connection.execute("PRAGMA incremental_vacuum").fetchall()
    if connection.execute("PRAGMA auto_vacuum").fetchone()[0] != 2:
        raise UpgradeError("incremental vacuum setting did not persist")
    candidates(connection)
    for _ in event_changes(connection):
        raise UpgradeError("event storage upgrade remains incomplete")
    if database_digest(connection, storage_settings=False) != before:
        raise UpgradeError("storage tuning changed logical database contents")


def upgrade(path, *, apply=False, confirm_stopped=False, backup_dir=None):
    if apply and (not confirm_stopped or backup_dir is None):
        raise UpgradeError("--apply requires --confirm-stopped and --backup-dir")
    if backup_dir is not None and not backup_dir.is_absolute():
        raise UpgradeError("backup directory must be absolute")
    if not path.is_absolute() or not path.is_file() or path.is_symlink():
        raise UpgradeError("checkpoint database must be an absolute existing regular file")
    path = path.resolve(strict=True)
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=1)) as source, source:
        source.execute("BEGIN")
        changes = candidates(source)
        original_digest = database_digest(source)
        source_schema = source.execute("PRAGMA user_version").fetchone()[0]
        event_count = deduplicated = bytes_removed = 0
        for _, _, original, replacement, referenced in event_changes(source):
            event_count += 1
            deduplicated += referenced
            bytes_removed += len(original.encode("utf-8")) - len(replacement.encode("utf-8"))
        vacuum_mode = source.execute("PRAGMA auto_vacuum").fetchone()[0]
        free_pages = source.execute("PRAGMA freelist_count").fetchone()[0]
        report = {
            "database": str(path), "accepted_checkpoint_versions": [OLD_VERSION, NEW_VERSION],
            "to_version": NEW_VERSION, "from_schema": source_schema, "to_schema": NEW_SCHEMA,
            "schema_changed": source_schema != NEW_SCHEMA,
            "from_auto_vacuum": vacuum_mode, "to_auto_vacuum": 2,
            "from_freelist_count": free_pages,
            "storage_tuning_changed": vacuum_mode != 2 or free_pages > 0,
            "vacuum_required": vacuum_mode == 0,
            "checkpoint_updates": len(changes),
            "event_envelopes_written": event_count, "tool_outputs_deduplicated": deduplicated,
            "event_json_bytes_removed": bytes_removed,
            "native_items_removed": sum(change[3] for change in changes),
            "plaintext_notes_restored": sum(change[4] for change in changes),
            "epoch_advances": sum(change[3] > 0 or change[4] for change in changes),
            "applied": False,
        }
        if not apply or (source_schema == NEW_SCHEMA and vacuum_mode == 2 and free_pages == 0):
            return report
        if backup_dir.is_symlink():
            raise UpgradeError("backup directory must not be a symlink")
        backup_dir.mkdir(mode=0o700, exist_ok=True)
        # Only the final directory may be new; its parent must already exist.
        sync_file_and_parent(backup_dir)
        if backup_dir.stat().st_mode & 0o077:
            raise UpgradeError("backup directory must be private (0700)")
        backup = backup_dir / f"{path.name}.before-portable-{uuid.uuid4().hex}.sqlite3"
        with closing(sqlite3.connect(backup)) as destination, destination:
            os.chmod(backup, 0o600)
            source.backup(destination)
            if destination.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                raise UpgradeError("checkpoint backup failed integrity verification")
            if database_digest(destination) != original_digest:
                raise UpgradeError("checkpoint backup does not match inspected state")
        sync_file_and_parent(backup)
    with closing(sqlite3.connect(path.as_uri() + "?mode=rw", uri=True, timeout=1)) as connection, connection:
        connection.execute("PRAGMA foreign_keys=ON")
        connection.execute("PRAGMA synchronous=FULL")
        connection.execute("BEGIN IMMEDIATE")
        if database_digest(connection) != original_digest:
            raise UpgradeError("database changed after inspection; stop gateway writes and retry")
        if source_schema == OLD_SCHEMA:
            # ALTER keeps every original session column and index intact. Defaults make
            # NOT NULL additions legal for populated tables; every row is filled below.
            connection.execute("ALTER TABLE sessions ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0 CHECK(context_epoch>=0)")
            connection.execute("ALTER TABLE sessions ADD COLUMN context_count INTEGER NOT NULL DEFAULT 0 CHECK(context_count>=0)")
            connection.execute(CONTEXT_TABLE)
            for session_id, original, replacement, _, _, epoch, items in changes:
                updated = connection.execute(
                    "UPDATE sessions SET latest_checkpoint_json=?, context_epoch=?, context_count=? "
                    "WHERE session_id=? AND latest_checkpoint_json=?",
                    (replacement, epoch, len(items), session_id, original),
                ).rowcount
                if updated != 1:
                    raise UpgradeError("checkpoint changed after inspection; stop gateway writes and retry")
                connection.executemany(
                    "INSERT INTO context_items(session_id,epoch,item_index,item_json) VALUES (?,?,?,?)",
                    ((session_id, epoch, index, item) for index, item in enumerate(items)),
                )
        if source_schema != NEW_SCHEMA:
            for session, sequence, original, replacement, _ in event_changes(connection):
                if connection.execute("UPDATE event_journal SET event_json=? WHERE session_id=? AND sequence=? AND event_json=?",
                                      (replacement, session, sequence, original)).rowcount != 1:
                    raise UpgradeError("event changed after inspection")
            connection.execute("PRAGMA user_version=13")
        candidates(connection)  # Verify the full target layout and rows before commit.
        for _ in event_changes(connection):
            raise UpgradeError("event storage upgrade remains incomplete")
        connection.commit()
        # VACUUM cannot join the schema transaction. A failure here never rolls
        # back an already committed upgrade or restores a possibly stale backup.
        try:
            tune_storage(connection)
        except (ValueError, sqlite3.Error, OSError) as error:
            report.update(storage_tuning_complete=False)
            try:
                write_receipt(report, backup)
            finally:
                raise UpgradeError(f"schema transaction committed; storage tuning incomplete or uncertain; verified backup retained at {backup}; keep stopped and rerun after inspection") from error
        report.update(storage_tuning_complete=True)
    return write_receipt(report, backup)


def write_receipt(report, backup):
    with backup.open("rb") as contents:
        digest = hashlib.sha256()
        for block in iter(lambda: contents.read(64 * 1024), b""):
            digest.update(block)
        backup_sha256 = digest.hexdigest()
    report.update(applied=True, backup=str(backup), backup_sha256=backup_sha256)
    receipt = backup.with_suffix(".receipt.json")
    with receipt.open("x") as output:
        os.chmod(receipt, 0o600)
        output.write(json.dumps(report, indent=2) + "\n")
        output.flush()
        os.fsync(output.fileno())
    sync_file_and_parent(receipt)
    return report


# Frozen released schema 11: never derive an old migration fixture from current DDL.
OLD_FIXTURE_SCHEMA = """
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS middleware_state (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (scope, key)
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    parent_session_id TEXT REFERENCES sessions(session_id),
    parent_sequence INTEGER CHECK (parent_sequence IS NULL OR parent_sequence >= 0),
    latest_sequence INTEGER NOT NULL CHECK (latest_sequence >= 0),
    latest_event_sequence INTEGER NOT NULL DEFAULT 0 CHECK (latest_event_sequence >= 0),
    latest_checkpoint_json TEXT NOT NULL,
    session_context_json TEXT NOT NULL,
    execution_stats_json TEXT NOT NULL,
    catalog_visible INTEGER NOT NULL CHECK (catalog_visible IN (0, 1)),
    first_user_message TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((parent_session_id IS NULL) = (parent_sequence IS NULL))
);
CREATE TABLE IF NOT EXISTS transcript_delta (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    items_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS message_receipts (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    submission_id TEXT NOT NULL,
    PRIMARY KEY (session_id, submission_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS execution_journal (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    record_json TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS event_journal (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= 0),
    event_kind TEXT NOT NULL,
    model_step_id TEXT,
    stream_phase TEXT,
    delta_bytes INTEGER CHECK (delta_bytes IS NULL OR delta_bytes >= 0),
    event_json TEXT NOT NULL,
    stream_metrics_json TEXT NOT NULL,
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS sessions_recent_idx
    ON sessions(updated_at DESC, latest_sequence DESC, session_id DESC);
CREATE INDEX IF NOT EXISTS execution_journal_recent_idx
    ON execution_journal(started_at_ms DESC, session_id DESC, sequence DESC);
CREATE INDEX IF NOT EXISTS event_journal_step_idx
    ON event_journal(session_id, model_step_id, event_kind);
PRAGMA user_version = 11;
COMMIT;
"""


def fixture_schema(connection):
    connection.executescript(OLD_FIXTURE_SCHEMA)


def fixture_checkpoint(connection, state):
    connection.execute("INSERT INTO sessions (session_id, latest_sequence, latest_checkpoint_json, "
                       "session_context_json, execution_stats_json, catalog_visible) VALUES (?, ?, ?, '{}', '{}', 1)",
                       (state["session_id"], state["sequence"], json.dumps(state)))


class UpgradeTests(unittest.TestCase):
    def test_latest_only_lossy_upgrade_and_idempotency(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "checkpoints.sqlite3"
            state = {
                "version": 18, "session_id": "session", "sequence": 42,
                "model_route": "source", "context_epoch": 9, "last_usage": {"input_tokens": 5},
                "last_context_rewrite": {"epoch": 9, "reasons": ["context_offloading"]},
                "context": [
                    {"type": "compaction", "encrypted_content": "native"},
                    {"type": "compaction_summary", "encrypted_content": "legacy-native"},
                    {"type": "reasoning", "encrypted_content": "ordinary"},
                    {"role": "user", "content": [{"type": "input_text", "text": "pending", CACHE_FIELD: True}]},
                    {"type": "function_call_output", "output": "[offloaded]"},
                ],
                "pending_messages": [{"id": "pending"}], "pending_approval": {"id": "approval"},
                "active_execution": {"submission_id": "active", "turn_id": "turn"},
                "active_model_step": {"model_step_id": "step"},
                "pending_tools": [{"call_id": "pending-tool", "name": "shell"}],
                "metadata": {"unrelated": "retained"}, "compaction_count": 3,
                "delivered_once": {"messages": ["permissions"]}, "total_usage": {"total_tokens": 100},
                "execution_stats": {"run_count": 7},
            }
            original = json.dumps(state)
            with closing(sqlite3.connect(path)) as connection, connection:
                fixture_schema(connection)
                fixture_checkpoint(connection, state)
                connection.execute("INSERT INTO transcript_delta (session_id,sequence,items_json) VALUES ('session',42,?)", (original,))
                connection.execute("INSERT INTO event_journal (session_id,sequence,recorded_at_ms,event_kind,event_json,stream_metrics_json) VALUES ('session',1,1,'test',?,'{}')", (original,))
                connection.execute("INSERT INTO execution_journal VALUES ('session',42,?,1)", (original,))
                connection.execute("INSERT INTO message_receipts VALUES ('session','receipt')")
                connection.execute("INSERT INTO middleware_state VALUES (?, ?, ?)", (
                    "session", "compaction.handoff", json.dumps("Previously saved plaintext notes.")
                ))
            dry_run = upgrade(path)
            self.assertEqual(dry_run["native_items_removed"], 2)
            self.assertEqual(dry_run["plaintext_notes_restored"], 1)
            with closing(sqlite3.connect(path)) as connection, connection:
                self.assertEqual(connection.execute("SELECT latest_checkpoint_json FROM sessions").fetchone()[0], original)
            result = upgrade(path, apply=True, confirm_stopped=True, backup_dir=Path(directory) / "backups")
            self.assertTrue(result["applied"])
            with closing(sqlite3.connect(path)) as connection, connection:
                upgraded = json.loads(connection.execute("SELECT latest_checkpoint_json FROM sessions").fetchone()[0])
                self.assertNotIn("context", upgraded)
                self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 13)
                upgraded["context"] = [json.loads(row[0]) for row in connection.execute(
                    "SELECT item_json FROM context_items ORDER BY item_index")]
                self.assertEqual(connection.execute("SELECT context_epoch,context_count FROM sessions").fetchone(), (10,4))
                self.assertEqual(connection.execute("SELECT latest_sequence FROM sessions").fetchone()[0], 42)
                self.assertEqual(connection.execute("SELECT items_json FROM transcript_delta").fetchone()[0], original)
                self.assertEqual(connection.execute("SELECT event_json FROM event_journal").fetchone()[0], '{"storage":"inline","event":' + original + '}')
                self.assertEqual(connection.execute("SELECT record_json FROM execution_journal").fetchone()[0], original)
                self.assertEqual(connection.execute("SELECT submission_id FROM message_receipts").fetchone()[0], "receipt")
                self.assertEqual(connection.execute("SELECT value_json FROM middleware_state").fetchone()[0],
                                 json.dumps("Previously saved plaintext notes."))
            self.assertEqual(upgraded["version"], 19)
            self.assertEqual(upgraded["context_model_route"], "source")
            self.assertEqual(upgraded["context_epoch"], 10)
            self.assertIsNone(upgraded["last_usage"])
            self.assertIsNone(upgraded["last_context_rewrite"])
            self.assertNotIn(CACHE_FIELD, upgraded["context"][1]["content"][0])
            self.assertEqual(upgraded["context"][0]["encrypted_content"], "ordinary")
            self.assertEqual(upgraded["context"][2]["output"], "[offloaded]")
            self.assertEqual(upgraded["context"][3]["content"][0]["text"],
                             restored_prompt() + "\n\nPreviously saved plaintext notes.")
            for field in ("pending_messages", "pending_approval", "delivered_once", "total_usage", "execution_stats",
                          "active_execution", "active_model_step", "pending_tools", "metadata", "compaction_count"):
                self.assertEqual(upgraded[field], state[field])
            backup = Path(result["backup"])
            self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
            self.assertEqual(backup.parent.stat().st_mode & 0o777, 0o700)
            self.assertEqual(backup.with_suffix(".receipt.json").stat().st_mode & 0o777, 0o600)
            with closing(sqlite3.connect(backup)) as connection, connection:
                self.assertEqual(connection.execute("SELECT latest_checkpoint_json FROM sessions").fetchone()[0], original)
            self.assertEqual(upgrade(path, apply=True, confirm_stopped=True, backup_dir=Path(directory) / "backups")["checkpoint_updates"], 0)

    def test_plaintext_upgrade_and_removed_rewrite_reason(self):
        state = {"version": 18, "model_route": "source", "context": [],
                 "context_epoch": 2, "last_usage": {"total_tokens": 3},
                 "last_context_rewrite": {"epoch": 2, "reasons": ["context_offloading", "scratchpad"]}}
        self.assertEqual(normalize(state), (True, 0, False))
        self.assertEqual(state["context_epoch"], 2)
        self.assertEqual(state["last_usage"], {"total_tokens": 3})
        self.assertEqual(state["last_context_rewrite"]["reasons"], ["scratchpad"])

    def test_existing_plaintext_projection_wins_over_legacy_state(self):
        state = {"version": 18, "model_route": "source", "context_epoch": 2,
                 "context": [{"role": "user", "_mobius_internal": "handoff_notes",
                              "content": [{"type": "input_text", "text": "durable"}]}]}
        self.assertEqual(normalize(state, "older side-store notes"), (True, 0, False))
        self.assertEqual(state["context_epoch"], 2)
        self.assertEqual(len(state["context"]), 1)
        self.assertEqual(state["context"][0]["content"][0]["text"], "durable")
        state.update(version=18, session_id="session", sequence=1)
        state.pop("context_model_route")
        with closing(sqlite3.connect(":memory:")) as connection, connection:
            fixture_schema(connection)
            fixture_checkpoint(connection, state)
            connection.execute("INSERT INTO middleware_state VALUES ('session', 'compaction.handoff', 'not-json')")
            self.assertEqual(candidates(connection)[0][3:5], (0, False))
            state["context"] = []
            connection.execute("UPDATE sessions SET latest_checkpoint_json=?", (json.dumps(state),))
            connection.execute("UPDATE middleware_state SET value_json='null'")
            with self.assertRaises(ValueError):
                candidates(connection)

    def test_unknown_schema_version_and_fields_refuse_before_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "checkpoints.sqlite3"
            state = {"version": 18, "session_id": "session", "sequence": 1,
                     "model_route": "source", "context_epoch": 0, "context": []}
            with closing(sqlite3.connect(path)) as connection, connection:
                fixture_schema(connection)
                fixture_checkpoint(connection, state)
                connection.execute("PRAGMA user_version=14")
            backups = Path(directory) / "backups"
            with self.assertRaises(ValueError):
                upgrade(path, apply=True, confirm_stopped=True, backup_dir=backups)
            self.assertFalse(backups.exists())
            with closing(sqlite3.connect(path)) as connection, connection:
                connection.execute("PRAGMA user_version=11")
                state["unexpected"] = "private fixture"
                connection.execute("UPDATE sessions SET latest_checkpoint_json=?", (json.dumps(state),))
            with self.assertRaises(ValueError):
                upgrade(path, apply=True, confirm_stopped=True, backup_dir=backups)
            self.assertFalse(backups.exists())

    def test_schema_11_version_19_split_and_empty_database(self):
        for populated in (True, False):
            with self.subTest(populated=populated), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "checkpoints.sqlite3"
                state = {"version": 19, "session_id": "session", "sequence": 9,
                         "model_route": "destination", "context_model_route": "original",
                         "context_epoch": 3, "context": [{"role": "user", "content": "retain"}]}
                with closing(sqlite3.connect(path)) as connection, connection:
                    fixture_schema(connection)
                    if populated:
                        fixture_checkpoint(connection, state)
                report = upgrade(path, apply=True, confirm_stopped=True,
                                 backup_dir=Path(directory) / "backups")
                self.assertTrue(report["schema_changed"] and report["applied"])
                self.assertEqual(report["checkpoint_updates"], int(populated))
                self.assertFalse(upgrade(path)["schema_changed"])
                with closing(sqlite3.connect(path)) as connection, connection:
                    self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 13)
                    if populated:
                        header = json.loads(connection.execute("SELECT latest_checkpoint_json FROM sessions").fetchone()[0])
                        self.assertEqual(header, {key: value for key, value in state.items() if key != "context"})
                        self.assertEqual(connection.execute("SELECT epoch,item_index,item_json FROM context_items").fetchone(),
                                         (3, 0, json.dumps(state["context"][0], separators=(",", ":"))))
                        connection.execute("UPDATE context_items SET item_index=2")
                if populated:
                    with self.assertRaises(UpgradeError):
                        upgrade(path)

    def test_current_runtime_schema_is_accepted_without_changes(self):
        source = (Path(__file__).resolve().parents[1] / "src/backend/checkpoint/sqlite.rs").read_text()
        schema = re.search(r'const SCHEMA: &str = "(.*?)";', source, re.S).group(1)
        with closing(sqlite3.connect(":memory:")) as connection, connection:
            connection.executescript(schema)
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 13)
            self.assertEqual(candidates(connection), [])
            original = database_digest(connection)
            connection.execute("PRAGMA user_version=11")
            self.assertNotEqual(database_digest(connection), original)

    def test_schema_and_rows_rollback_together_on_final_validation_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "checkpoints.sqlite3"
            state = {"version": 19, "session_id": "session", "sequence": 1,
                     "model_route": "source", "context_model_route": "source",
                     "context_epoch": 0, "context": [{"role": "user", "content": "retained"}]}
            with closing(sqlite3.connect(path)) as connection, connection:
                fixture_schema(connection)
                fixture_checkpoint(connection, state)
                connection.execute("INSERT INTO event_journal(session_id,sequence,recorded_at_ms,event_kind,event_json,stream_metrics_json) VALUES ('session',1,1,'test','{\"msg\":{\"type\":\"custom\"}}','{}')")
                original = database_digest(connection)
            validate = candidates
            def fail_target(connection):
                if connection.execute("PRAGMA user_version").fetchone()[0] == 13:
                    raise UpgradeError("injected final validation failure")
                return validate(connection)
            with mock.patch(__name__ + ".candidates", side_effect=fail_target):
                with self.assertRaises(UpgradeError):
                    upgrade(path, apply=True, confirm_stopped=True,
                            backup_dir=Path(directory) / "backups")
            with closing(sqlite3.connect(path)) as connection, connection:
                self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 11)
                self.assertEqual(database_digest(connection), original)

    def test_current_schema_storage_tuning_and_idempotence(self):
        for mode in (0, 1, 2):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "checkpoints.sqlite3"
                source = (Path(__file__).resolve().parents[1] / "src/backend/checkpoint/sqlite.rs").read_text()
                schema = re.search(r'const SCHEMA: &str = "(.*?)";', source, re.S).group(1)
                with closing(sqlite3.connect(path)) as connection, connection:
                    connection.execute(f"PRAGMA auto_vacuum={mode}")
                    connection.executescript(schema)
                    connection.execute(f"PRAGMA auto_vacuum={mode}")
                    connection.execute("VACUUM")
                    original = database_digest(connection, storage_settings=False)
                dry = upgrade(path)
                self.assertEqual(dry["vacuum_required"], mode == 0)
                self.assertEqual(dry["storage_tuning_changed"], mode != 2)
                self.assertFalse(dry["applied"])
                report = upgrade(path, apply=True, confirm_stopped=True,
                                 backup_dir=Path(directory) / "backups")
                self.assertFalse(report["schema_changed"])
                self.assertEqual(report["applied"], mode != 2)
                with closing(sqlite3.connect(path)) as connection:
                    self.assertEqual(connection.execute("PRAGMA auto_vacuum").fetchone()[0], 2)
                    self.assertEqual(database_digest(connection, storage_settings=False), original)
                self.assertFalse(upgrade(path)["storage_tuning_changed"])

    def test_vacuum_failure_keeps_committed_schema_and_retryable_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "checkpoints.sqlite3"
            with closing(sqlite3.connect(path)) as connection, connection:
                fixture_schema(connection)
            original_tune = tune_storage
            def denied_vacuum(connection):
                connection.set_authorizer(lambda action, *args: sqlite3.SQLITE_DENY
                                          if action == sqlite3.SQLITE_ATTACH else sqlite3.SQLITE_OK)
                original_tune(connection)
            backups = Path(directory) / "backups"
            with mock.patch(__name__ + ".tune_storage", side_effect=denied_vacuum):
                with self.assertRaisesRegex(UpgradeError, "schema transaction committed"):
                    upgrade(path, apply=True, confirm_stopped=True, backup_dir=backups)
            receipt = json.loads(next(backups.glob("*.receipt.json")).read_text())
            self.assertTrue(receipt["applied"])
            self.assertFalse(receipt["storage_tuning_complete"])
            with closing(sqlite3.connect(path)) as connection:
                self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 13)
                self.assertEqual(connection.execute("PRAGMA auto_vacuum").fetchone()[0], 0)
            self.assertTrue(upgrade(path, apply=True, confirm_stopped=True,
                                    backup_dir=backups)["storage_tuning_complete"])

    def test_historical_event_dedup_is_exact_session_scoped_and_lossless(self):
        for source_schema in (11, 12):
            with self.subTest(source_schema=source_schema), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "checkpoints.sqlite3"
                output = [{"type": "input_text", "text": "é" * 3000},
                          {"type": "input_text", "text": "tail"}]
                item = {"type": "function_call_output", "call_id": "call",
                        "_mobius_is_error": False, "output": output}
                message = {"type": "tool_call_end", "turn_id": "turn", "call_id": "call",
                           "name": "read_file", "is_error": False, "output": output}
                events = [{"msg": message}, {"submission_id": "submission", "msg": message},
                          {"msg": dict(message, is_error=True)},
                          {"msg": dict(message, custom="preserved")},
                          {"msg": dict(message, output=[{"type": "input_text", "text": "different"}])}]
                with closing(sqlite3.connect(path)) as connection, connection:
                    fixture_schema(connection)
                    for session in ("unique", "ambiguous", "no_match"):
                        fixture_checkpoint(connection, {"version": 19, "session_id": session,
                            "sequence": 1, "context_epoch": 0, "context": [],
                            "model_route": "source", "context_model_route": "source"})
                    if source_schema == 12:
                        connection.execute("ALTER TABLE sessions ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0")
                        connection.execute("ALTER TABLE sessions ADD COLUMN context_count INTEGER NOT NULL DEFAULT 0")
                        connection.execute(CONTEXT_TABLE)
                        for session, raw in connection.execute("SELECT session_id,latest_checkpoint_json FROM sessions").fetchall():
                            header = json.loads(raw)
                            del header["context"]
                            connection.execute("UPDATE sessions SET latest_checkpoint_json=? WHERE session_id=?", (json.dumps(header), session))
                        connection.execute("PRAGMA user_version=12")
                    for session, items in (("unique", [item]), ("ambiguous", [item, item])):
                        connection.execute("INSERT INTO transcript_delta(session_id,sequence,items_json) VALUES (?,1,?)",
                                           (session, json.dumps(items)))
                    for session in ("unique", "ambiguous", "no_match"):
                        for index, event in enumerate(events, 1):
                            connection.execute("INSERT INTO event_journal(session_id,sequence,recorded_at_ms,event_kind,event_json,stream_metrics_json) VALUES (?,?,1,'tool_call_end',?,'{}')",
                                               (session, index, json.dumps(event)))
                dry = upgrade(path)
                self.assertEqual(dry["event_envelopes_written"], 15)
                self.assertEqual(dry["tool_outputs_deduplicated"], 2)
                report = upgrade(path, apply=True, confirm_stopped=True,
                                 backup_dir=Path(directory) / "backups")
                self.assertEqual(report["tool_outputs_deduplicated"], 2)
                self.assertGreater(report["event_json_bytes_removed"], 8000)
                self.assertEqual(upgrade(path)["event_envelopes_written"], 0)
                with closing(sqlite3.connect(path)) as connection, connection:
                    for session, sequence, raw in connection.execute("SELECT session_id,sequence,event_json FROM event_journal"):
                        stored = json.loads(raw)
                        if session == "unique" and sequence <= 2:
                            self.assertEqual(stored["storage"], "tool_output")
                            saved = json.loads(connection.execute("SELECT items_json FROM transcript_delta WHERE session_id=? AND sequence=?", (session, stored["transcript_sequence"])).fetchone()[0])[stored["item_index"]]
                            rebuilt = {"msg": {"type": "tool_call_end", **{key: stored[key] for key in ("turn_id", "call_id", "name", "is_error")}, "output": saved["output"]}}
                            if stored["submission_id"] is not None:
                                rebuilt["submission_id"] = stored["submission_id"]
                            self.assertEqual(rebuilt, events[sequence - 1])
                        else:
                            self.assertEqual(stored, {"storage": "inline", "event": events[sequence - 1]})
                    connection.execute("DELETE FROM transcript_delta WHERE session_id='unique'")
                with self.assertRaisesRegex(UpgradeError, "reference is missing"):
                    upgrade(path)

    def test_existing_incremental_storage_reclaims_free_pages_without_changing_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "checkpoints.sqlite3"
            source = (Path(__file__).resolve().parents[1] / "src/backend/checkpoint/sqlite.rs").read_text()
            schema = re.search(r'const SCHEMA: &str = "(.*?)";', source, re.S).group(1)
            with closing(sqlite3.connect(path)) as connection, connection:
                connection.execute("PRAGMA auto_vacuum=INCREMENTAL")
                connection.executescript(schema)
                connection.execute("PRAGMA user_version=12")
                connection.execute("INSERT INTO middleware_state(scope,key,value_json) VALUES ('fixture','large',?)",
                                   (json.dumps("x" * (1024 * 1024)),))
                connection.commit()
                connection.execute("DELETE FROM middleware_state WHERE scope='fixture'")
                connection.commit()
                free_before = connection.execute("PRAGMA freelist_count").fetchone()[0]
                pages_before = connection.execute("PRAGMA page_count").fetchone()[0]
                self.assertGreater(free_before, 100)
                connection.execute("PRAGMA user_version=13")
                target_digest = database_digest(connection)
                connection.execute("PRAGMA user_version=12")
            original_tune = tune_storage
            def denied_incremental(connection):
                connection.set_authorizer(lambda action, name, *args: sqlite3.SQLITE_DENY
                                          if action == sqlite3.SQLITE_PRAGMA and name == "incremental_vacuum"
                                          else sqlite3.SQLITE_OK)
                original_tune(connection)
            with mock.patch(__name__ + ".tune_storage", side_effect=denied_incremental):
                with self.assertRaisesRegex(UpgradeError, "schema transaction committed"):
                    upgrade(path, apply=True, confirm_stopped=True,
                            backup_dir=Path(directory) / "backups")
            dry = upgrade(path)
            self.assertFalse(dry["schema_changed"])
            self.assertTrue(dry["storage_tuning_changed"])
            self.assertGreater(dry["from_freelist_count"], 0)
            report = upgrade(path, apply=True, confirm_stopped=True,
                             backup_dir=Path(directory) / "backups")
            self.assertFalse(report["vacuum_required"])
            with closing(sqlite3.connect(path)) as connection:
                self.assertEqual(connection.execute("PRAGMA freelist_count").fetchone()[0], 0)
                self.assertLess(connection.execute("PRAGMA page_count").fetchone()[0], pages_before)
                self.assertEqual(database_digest(connection), target_digest)
            self.assertFalse(upgrade(path, apply=True, confirm_stopped=True,
                                     backup_dir=Path(directory) / "backups")["applied"])

    def test_stopped_backup_and_owner_validation(self):
        for arguments in ({"apply": True}, {"apply": True, "backup_dir": Path("/not-used")},
                          {"apply": True, "confirm_stopped": True}):
            with self.assertRaises(ValueError):
                upgrade(Path("/does-not-exist"), **arguments)
        for state in (
            {"version": 19, "context": []},
            {"version": 19, "context": [], "context_model_route": ""},
            {"version": 19, "context": [], "context_model_route": 3},
            {"version": 18, "context": [{}], "model_route": None},
            {"version": 18, "context": [], "model_route": "source", "context_model_route": "source"},
        ):
            with self.assertRaises(ValueError):
                normalize(state)
        empty = {"version": 18, "context": [], "model_route": None, "context_epoch": 0}
        self.assertEqual(normalize(empty), (True, 0, False))
        self.assertIsNone(empty["context_model_route"])
        self.assertEqual(normalize(empty), (False, 0, False))
        overflow = {"version": 18, "context": [{"type": "compaction"}],
                    "model_route": "source", "context_epoch": MAX_EPOCH}
        with self.assertRaises(ValueError):
            normalize(overflow)
        self.assertEqual(overflow["context_epoch"], MAX_EPOCH)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, action="append", default=[])
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--confirm-stopped", action="store_true")
    parser.add_argument("--backup-dir", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.database or args.apply or args.backup_dir:
            parser.error("self-test does not accept real paths or apply")
        unittest.main(argv=[__file__])
        return
    if not args.database:
        parser.error("at least one --database is required")
    if args.apply and (not args.confirm_stopped or args.backup_dir is None):
        parser.error("--apply requires --confirm-stopped and --backup-dir")
    if args.backup_dir is not None and not args.backup_dir.is_absolute():
        parser.error("--backup-dir must be an absolute private path")
    try:
        # Refuse a later unsupported database before modifying any earlier target.
        for path in args.database:
            upgrade(path)
        for path in args.database:
            print(json.dumps(upgrade(path, apply=args.apply, confirm_stopped=args.confirm_stopped,
                                     backup_dir=args.backup_dir)))
    except (ValueError, KeyError, TypeError, OSError, sqlite3.Error) as error:
        # Decoder/SQLite errors can include user data; only our fixed validation messages are safe.
        message = str(error) if isinstance(error, UpgradeError) else type(error).__name__
        parser.exit(1, f"Upgrade refused or interrupted: {message}. Keep the gateway stopped; inspect verified backups before restarting.\n")


if __name__ == "__main__":
    main()
