#!/usr/bin/env python3
"""Offline portable-compaction config upgrade; never run on a live gateway.

Dry run: --gateway-config /absolute/gateway.toml --bots-db /absolute/bots.sqlite3
Apply:   same flags plus --apply --confirm-stopped --backup-dir /absolute/new-backup
Requires Python 3.11+. Self-test uses synthetic temporary files only. Run the checkpoint upgrade separately,
then the new gateway's check-config before restarting. No credentials are printed.
"""

import argparse
import copy
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import tempfile
import uuid
try:
    import tomllib
except ModuleNotFoundError:
    raise SystemExit("requires Python 3.11+ (for example python3.13)")


class UpgradeError(ValueError):
    """Fixed validation messages that never embed input contents."""


REMOVED_COMPACTION = {
    "keep_recent_tokens", "native_retained_tokens", "handoff_reserve_divisor",
    "handoff_warning_reserves", "handoff_urgent_reserves",
}
REMOVED_TRANSPORT = {"compaction_retry_limit", "compaction_retry_backoff_ms"}
COMPACTION_TABLE = "bot_defaults.config.middleware.settings.compaction"
OFFLOADING_TABLE = "bot_defaults.config.middleware.settings.context_offloading"
MIDDLEWARE_TABLE = "bot_defaults.config.middleware"
SECTION = re.compile(r"^\s*\[([a-zA-Z0-9_.]+)\]\s*(?:#.*)?$")
ASSIGNMENT = re.compile(r"^(\s*)([a-zA-Z0-9_]+)(\s*=\s*)(.*)$")


def require(condition, message):
    if not condition:
        raise UpgradeError(message)


def positive(value, name):
    require(type(value) is int and 0 < value < 2**64, f"{name} must be a positive u64")


def migrate_composition(versioned):
    require(isinstance(versioned, dict) and set(versioned) == {"revision", "config"},
            "invalid versioned bot config")
    positive(versioned["revision"], "bot revision")
    composition = versioned["config"]
    require(isinstance(composition, dict) and set(composition) <= {
        "provider", "realtime_voice", "middleware", "extensions", "system_prompt", "max_model_steps",
    }, "unknown or malformed bot composition")
    require(isinstance(composition.get("provider"), dict), "missing bot provider")
    require(isinstance(composition.get("system_prompt"), str), "missing bot system prompt")
    positive(composition.get("max_model_steps"), "max_model_steps")
    middleware = composition.get("middleware")
    require(isinstance(middleware, dict) and set(middleware) == {"enabled", "settings"},
            "invalid middleware config")
    enabled = middleware["enabled"]
    require(isinstance(enabled, list) and all(isinstance(item, str) and
            re.fullmatch(r"[a-z_][a-z0-9_]*", item) for item in enabled) and
            len(set(enabled)) == len(enabled), "invalid enabled middleware list")
    settings = middleware["settings"]
    require(isinstance(settings, dict) and all(isinstance(owner, str) and
            isinstance(values, dict) for owner, values in settings.items()),
            "invalid middleware settings")
    before = copy.deepcopy(middleware)
    compaction = settings.get("compaction")
    require(compaction is not None and ("mode" in compaction or "allow_model_compaction" in compaction),
            "missing compaction selection; no default is guessed")
    if compaction is not None:
        require(set(compaction) <= REMOVED_COMPACTION | {
            "mode", "at_tokens", "reserve_tokens", "allow_model_compaction",
        }, "unknown compaction settings")
        for name, value in compaction.items():
            if name not in {"mode", "allow_model_compaction"}:
                positive(value, f"compaction.{name}")
        if "allow_model_compaction" in compaction:
            require(compaction["allow_model_compaction"] in ("on", "off"),
                    "allow_model_compaction must be on or off")
            require("mode" not in compaction, "mixed old/new compaction selection")
        if "mode" in compaction:
            require(compaction["mode"] in ("automatic", "handoff"), "unknown compaction mode")
            compaction["allow_model_compaction"] = {
                "automatic": "off", "handoff": "on",
            }[compaction.pop("mode")]
        for name in REMOVED_COMPACTION:
            compaction.pop(name, None)
    offloading = settings.get("context_offloading")
    if offloading is not None:
        require(set(offloading) <= {"stale_after_tokens"}, "unknown context_offloading settings")
        if "stale_after_tokens" in offloading:
            positive(offloading["stale_after_tokens"], "context_offloading.stale_after_tokens")
        del settings["context_offloading"]
    enabled[:] = [item for item in enabled if item != "context_offloading"]
    return middleware != before


def migrate_config(config):
    """Mutate the parsed gateway config; return whether this one-off changed it."""
    require(isinstance(config, dict) and type(config.get("version")) is int and config["version"] in (26, 27),
            "expected gateway config version 26 or 27")
    require(set(config) <= {"version", "runtime", "connections", "auth", "computer",
            "model_transport", "execution", "telemetry", "listen", "tls", "cloudflare",
            "desktop_enabled", "bot_defaults", "configured_providers", "installed_extensions", "usage"},
            "unknown gateway configuration fields")
    original = copy.deepcopy(config)
    changed = False
    transport = config.get("model_transport", {})
    require(isinstance(transport, dict), "invalid model_transport table")
    transport_defaults = tomllib.loads((Path(__file__).resolve().parents[1] /
                                        "src/backend/model/transport.toml").read_text())
    require(set(transport) <= set(transport_defaults) | REMOVED_TRANSPORT,
            "unknown transport settings")
    for name in REMOVED_TRANSPORT:
        if name in transport:
            value = transport[name]
            require(type(value) is int and 0 <= value < 2**64,
                    f"model_transport.{name} must be a nonnegative integer")
            del transport[name]
            changed = True
    defaults = config.get("bot_defaults")
    if defaults is not None:
        changed = migrate_composition(defaults) or changed
    if original["version"] == 27:
        require(not changed, "version 27 still contains retired settings")
        return False
    config["version"] = 27
    return True


def rewrite_toml(text, expected):
    """Patch only canonical gateway fields; parsed equality fails closed on other layouts."""
    lines = text.splitlines(keepends=True)
    output = []
    section = ""
    index = 0
    while index < len(lines):
        line = lines[index]
        header = SECTION.fullmatch(line.rstrip("\r\n"))
        if header:
            section = header[1]
        if section == OFFLOADING_TABLE:
            index += 1
            continue
        assignment = ASSIGNMENT.fullmatch(line.rstrip("\r\n"))
        if assignment:
            indent, name, equals, value = assignment.groups()
            if not section and name == "version":
                comment = " #" + value.partition("#")[2] if "#" in value else ""
                output.append(indent + "version" + equals + str(expected["version"]) + comment + "\n")
                index += 1
                continue
            if (section == "model_transport" and name in REMOVED_TRANSPORT) or (
                    section == COMPACTION_TABLE and name in REMOVED_COMPACTION):
                # Retain operator comments even when their obsolete field is removed.
                if "#" in value:
                    output.append(indent + "#" + value.partition("#")[2] + "\n")
                index += 1
                continue
            if section == COMPACTION_TABLE and name == "mode":
                require(re.fullmatch(r"(?:\"(?:automatic|handoff)\"|'(?:automatic|handoff)')\s*(?:#.*)?", value),
                        "unsupported TOML mode formatting; no files changed")
                comment = " #" + value.partition("#")[2] if "#" in value else ""
                selected = expected["bot_defaults"]["config"]["middleware"]["settings"]["compaction"]["allow_model_compaction"]
                output.append(indent + "allow_model_compaction" + equals + json.dumps(selected) + comment + "\n")
                index += 1
                continue
            if section == MIDDLEWARE_TABLE and name == "enabled" and value.lstrip().startswith("["):
                # Multiline canonical arrays may carry the removed entry on later lines.
                end = index
                block = line
                while "]" not in block:
                    end += 1
                    require(end < len(lines), "unterminated middleware enabled list")
                    block += lines[end]
                if "context_offloading" in block:
                    enabled = expected["bot_defaults"]["config"]["middleware"]["enabled"]
                    output.append(indent + "enabled" + equals + json.dumps(enabled) + "\n")
                    for original in block.splitlines():
                        if "#" in original:
                            output.append(indent + "#" + original.partition("#")[2] + "\n")
                    index = end + 1
                    continue
        output.append(line)
        index += 1
    rewritten = "".join(output)
    require(tomllib.loads(rewritten) == expected,
            "unsupported TOML formatting; no files changed (use canonical gateway tables)")
    return rewritten


def json_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON key in catalog")
        result[key] = value
    return result


def reject_nonfinite(_value):
    raise UpgradeError("nonfinite catalog JSON")


def migrate_catalog(raw):
    state = json.loads(raw, object_pairs_hook=json_object,
                       parse_constant=reject_nonfinite)
    require(isinstance(state, dict) and set(state) == {
        "version", "bots", "routines", "pending_bot_deletion",
    } and type(state["version"]) is int and state["version"] in (7, 8), "expected BotState version 7 or 8")
    require(isinstance(state["bots"], list) and isinstance(state["routines"], list), "invalid BotState lists")
    count = 0
    identities = set()
    for bot in state["bots"]:
        require(isinstance(bot, dict) and set(bot) == {
            "id", "handle", "name", "description", "tint", "shape", "config",
        }, "unknown or malformed StoredBot")
        require(isinstance(bot["id"], str) and bot["id"] not in identities,
                "invalid or duplicate stored bot ID")
        require(str(uuid.UUID(bot["id"])) == bot["id"], "stored bot ID is not canonical")
        identities.add(bot["id"])
        count += migrate_composition(bot["config"])
    if state["version"] == 8:
        require(count == 0, "version 8 still contains retired settings")
        return raw, 0
    state["version"] = 8
    return json.dumps(state, separators=(",", ":"), ensure_ascii=False), count


def digest(data):
    return hashlib.sha256(data).hexdigest()


def database(path, readonly=True):
    require(path.is_file() and not path.is_symlink(), "bots database must be an existing regular file")
    connection = sqlite3.connect(path.as_uri() + ("?mode=ro" if readonly else "?mode=rw"), uri=True, timeout=1)
    try:
        require(connection.execute("PRAGMA user_version").fetchone()[0] == 6,
                "expected bots.sqlite3 schema 6")
        require(connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok", "bots database integrity failed")
        require(not connection.execute("SELECT 1 FROM sqlite_master WHERE type='trigger'").fetchone(),
                "unexpected database triggers")
        require(connection.execute("PRAGMA table_info(catalog)").fetchall() == [
            (0, "id", "INTEGER", 0, None, 1), (1, "state_json", "TEXT", 1, None, 0),
        ], "unexpected catalog table schema")
        return connection
    except Exception:
        connection.close()
        raise


def database_digest(connection):
    digest = hashlib.sha256()
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


def atomic_write(path, contents):
    descriptor, temporary = tempfile.mkstemp(prefix=".portable-config-", dir=path.parent)
    try:
        original = path.stat()
        os.fchown(descriptor, original.st_uid, original.st_gid)
        with os.fdopen(descriptor, "wb") as output:
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        parent = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
    finally:
        Path(temporary).unlink(missing_ok=True)


def run(config_path, bots_path, apply=False, confirm_stopped=False, backup_dir=None):
    require(not apply or (confirm_stopped and backup_dir is not None),
            "--apply requires --confirm-stopped and --backup-dir")
    require(config_path is not None or bots_path is not None, "provide --gateway-config and/or --bots-db")
    require(all(path is None or path.is_absolute() for path in (config_path, bots_path)),
            "input paths must be absolute")
    require(config_path is None or (config_path.is_file() and not config_path.is_symlink()),
            "gateway config must be an existing regular file")
    original_config = config_path.read_bytes() if config_path else None
    replacement_config = original_config
    if config_path:
        expected = tomllib.loads(original_config.decode("utf-8"))
        if migrate_config(expected):
            replacement_config = rewrite_toml(original_config.decode("utf-8"), expected).encode("utf-8")
    connection = database(bots_path) if bots_path else None
    try:
        if connection:
            connection.execute("BEGIN")
        original_db_digest = database_digest(connection) if connection else None
        row = connection.execute("SELECT state_json FROM catalog WHERE id = 1").fetchone() if connection else None
        require(connection is None or row is not None, "missing Bot catalog row")
        original_catalog = row[0] if row else None
        replacement_catalog, changed_bots = migrate_catalog(original_catalog) if row else (None, 0)
        receipt = {
            "mode": "apply" if apply else "dry_run",
            "config_changed": original_config != replacement_config,
            "changed_bots": changed_bots,
            "catalog_changed": original_catalog != replacement_catalog,
            "config_sha256_before": digest(original_config) if config_path else None,
            "config_sha256_after": digest(replacement_config) if config_path else None,
            "catalog_sha256_before": digest(original_catalog.encode()) if row else None,
            "catalog_sha256_after": digest(replacement_catalog.encode()) if row else None,
        }
        if not apply or not (receipt["config_changed"] or receipt["catalog_changed"]):
            return receipt
        require(backup_dir.is_absolute(), "backup directory must be absolute")
        backup_dir.mkdir(mode=0o700, exist_ok=False)
        # Persist the new directory entry before relying on any backup inside it.
        sync_file_and_parent(backup_dir)
        if config_path:
            backup_config = backup_dir / "gateway.toml"
            with backup_config.open("xb") as output:
                os.chmod(backup_config, 0o600)
                output.write(original_config)
                output.flush()
                os.fsync(output.fileno())
            require(backup_config.read_bytes() == original_config, "config backup verification failed")
            sync_file_and_parent(backup_config)
        if connection:
            backup_db = backup_dir / "bots.sqlite3"
            with closing(sqlite3.connect(backup_db)) as backup, backup:
                os.chmod(backup_db, 0o600)
                connection.backup(backup)
                require(backup.execute("PRAGMA integrity_check").fetchone()[0] == "ok", "database backup integrity failed")
                require(database_digest(backup) == original_db_digest,
                        "database backup content mismatch")
            sync_file_and_parent(backup_db)
            connection.close()
            connection = database(bots_path, readonly=False)
            connection.execute("BEGIN IMMEDIATE")
            require(database_digest(connection) == original_db_digest,
                    "database changed after inspection; stop the gateway")
        require(config_path is None or config_path.read_bytes() == original_config,
                "config changed after dry run; stop the gateway")
        try:
            if receipt["config_changed"]:
                atomic_write(config_path, replacement_config)
            if receipt["catalog_changed"]:
                cursor = connection.execute("UPDATE catalog SET state_json = ? WHERE id = 1", (replacement_catalog,))
                require(cursor.rowcount == 1, "missing catalog row during apply")
            if connection:
                connection.commit()
        except Exception:
            if connection:
                connection.rollback()
            # Never guess whether an fsync/commit failure published a change.
            # Keep verified originals for explicit all-resource restoration while stopped.
            raise
        with (backup_dir / "receipt.json").open("x") as output:
            os.chmod(output.name, 0o600)
            json.dump(receipt, output, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        sync_file_and_parent(backup_dir / "receipt.json")
        return receipt
    finally:
        if connection:
            connection.close()


def self_test():
    config_text = '''# operator comment
version = 26
listen = "127.0.0.1:8741"
[model_transport]
stream_retry_limit = 5
compaction_retry_limit = 2 # old retry
compaction_retry_backoff_ms = 200
[bot_defaults]
revision = 3
[bot_defaults.config]
system_prompt = "Keep this text exactly # unchanged"
max_model_steps = 100
extensions = []
[bot_defaults.config.provider]
provider = "openai"
model = "test"
[bot_defaults.config.middleware]
enabled = [
  "compaction", # keep comment
  "context_offloading", # removed capability
  "tasks",
]
[bot_defaults.config.middleware.settings.compaction]
mode = "automatic" # existing choice
at_tokens = 250000
reserve_tokens = 16384
keep_recent_tokens = 20000
native_retained_tokens = 64000
handoff_reserve_divisor = 8
handoff_warning_reserves = 3
handoff_urgent_reserves = 2
[bot_defaults.config.middleware.settings.context_offloading]
stale_after_tokens = 10000
[bot_defaults.config.middleware.settings.tasks]
max_items = 30
'''
    with tempfile.TemporaryDirectory(prefix="portable-config-self-test-") as temporary:
        directory = Path(temporary)
        config_path, bots_path = directory / "gateway.toml", directory / "bots.sqlite3"
        config_path.write_text(config_text)
        automatic = tomllib.loads(config_text)["bot_defaults"]
        handoff = copy.deepcopy(automatic)
        handoff["config"]["middleware"]["settings"]["compaction"]["mode"] = "handoff"
        state = {
            "version": 7, "routines": [{"unrelated": "preserved"}], "pending_bot_deletion": None,
            "bots": [dict(id=str(uuid.UUID(int=index + 1)), handle=f"bot{index}", name="Synthetic", description="",
                          tint="blue", shape="circle", config=value)
                     for index, value in enumerate((automatic, handoff))],
        }
        raw = json.dumps(state)
        with closing(sqlite3.connect(bots_path)) as connection, connection:
            connection.executescript("PRAGMA user_version=6; CREATE TABLE catalog (id INTEGER PRIMARY KEY CHECK(id=1), state_json TEXT NOT NULL); CREATE TABLE untouched (value TEXT);")
            connection.execute("INSERT INTO catalog VALUES (1, ?)", (raw,))
            connection.execute("INSERT INTO untouched VALUES ('retain')")
        dry = run(config_path, bots_path)
        assert dry["config_changed"] and dry["changed_bots"] == 2
        assert config_path.read_text() == config_text
        with closing(database(bots_path)) as connection, connection:
            assert connection.execute("SELECT state_json FROM catalog").fetchone()[0] == raw
        for arguments in [(True, False, directory / "bad"), (True, True, None)]:
            try:
                run(config_path, bots_path, *arguments)
            except ValueError:
                pass
            else:
                raise AssertionError("missing stopped/backup guard")
        backup = directory / "backup"
        run(config_path, bots_path, True, True, backup)
        upgraded = tomllib.loads(config_path.read_text())
        assert upgraded["bot_defaults"]["config"]["middleware"]["settings"]["compaction"] == {
            "allow_model_compaction": "off", "at_tokens": 250000, "reserve_tokens": 16384,
        }
        assert "# keep comment" in config_path.read_text() and "# old retry" in config_path.read_text()
        assert upgraded["version"] == 27
        assert upgraded["model_transport"] == {"stream_retry_limit": 5}
        with closing(database(bots_path)) as connection, connection:
            saved = json.loads(connection.execute("SELECT state_json FROM catalog").fetchone()[0])
            assert [bot["config"]["config"]["middleware"]["settings"]["compaction"]["allow_model_compaction"] for bot in saved["bots"]] == ["off", "on"]
            assert saved["version"] == 8
            assert saved["routines"] == state["routines"]
            assert [bot["config"]["revision"] for bot in saved["bots"]] == [3, 3]
            assert connection.execute("SELECT value FROM untouched").fetchone()[0] == "retain"
            assert connection.execute("PRAGMA user_version").fetchone()[0] == 6
        repeated = run(config_path, bots_path, True, True, directory / "unused-backup")
        assert not repeated["config_changed"] and repeated["changed_bots"] == 0
        assert not (directory / "unused-backup").exists()
        assert (backup / "gateway.toml").read_text() == config_text
        with closing(sqlite3.connect(backup / "bots.sqlite3")) as connection, connection:
            assert connection.execute("SELECT state_json FROM catalog").fetchone()[0] == raw
        assert backup.stat().st_mode & 0o777 == 0o700
        assert all(path.stat().st_mode & 0o777 == 0o600 for path in backup.iterdir())
        for invalid in ("bogus", 123):
            fixture = tomllib.loads(config_text)
            fixture["bot_defaults"]["config"]["middleware"]["settings"]["compaction"]["mode"] = invalid
            try:
                migrate_config(fixture)
            except ValueError:
                pass
            else:
                raise AssertionError("invalid mode accepted")
        for document in (config_text.replace("keep_recent_tokens = 20000", "unknown_setting = 3"),
                         config_text.replace("version = 26", "version = 99"),
                         config_text.replace('mode = "automatic" # existing choice\n', "")):
            try:
                migrate_config(tomllib.loads(document))
            except ValueError:
                pass
            else:
                raise AssertionError("unknown config accepted")
        for enabled in ('enabled = ["compaction", "context_offloading", "tasks"]',
                        "enabled = ['compaction', 'context_offloading', 'tasks']"):
            document = re.sub(r"enabled = \[.*?\]", enabled, config_text, count=1, flags=re.S)
            expected = tomllib.loads(document)
            assert migrate_config(expected)
            assert tomllib.loads(rewrite_toml(document, expected)) == expected
        for invalid in (dict(state, version=99), dict(state, unexpected=True)):
            try:
                migrate_catalog(json.dumps(invalid))
            except ValueError:
                pass
            else:
                raise AssertionError("unknown BotState accepted")
        mixed = copy.deepcopy(state)
        mixed["bots"][0]["config"]["config"]["middleware"]["settings"]["compaction"]["allow_model_compaction"] = "off"
        try:
            migrate_catalog(json.dumps(mixed))
        except ValueError:
            pass
        else:
            raise AssertionError("mixed old/new compaction settings accepted")
        # Generation-only upgrades must persist even when no Bot settings change.
        empty = dict(version=7, bots=[], routines=[], pending_bot_deletion=None)
        upgraded_raw, count = migrate_catalog(json.dumps(empty))
        assert count == 0 and json.loads(upgraded_raw)["version"] == 8
        with closing(sqlite3.connect(bots_path)) as connection, connection:
            connection.execute("UPDATE catalog SET state_json=?", (json.dumps(empty),))
        generation = run(None, bots_path, True, True, directory / "empty-backup")
        assert generation["catalog_changed"] and generation["changed_bots"] == 0
        assert not run(None, bots_path)["catalog_changed"]
        # Unsupported formatting and bad catalogs fail before creating backups or writes.
        quote_headers = config_text.replace("[bot_defaults.config.middleware.settings.compaction]",
                                            '[bot_defaults.config.middleware.settings."compaction"]')
        config_path.write_text(quote_headers)
        try:
            run(config_path, bots_path, True, True, directory / "rejected-backup")
        except ValueError:
            pass
        else:
            raise AssertionError("unsupported formatting accepted")
        assert config_path.read_text() == quote_headers
        assert not (directory / "rejected-backup").exists()
        config_path.write_text(config_text)
        with closing(sqlite3.connect(bots_path)) as connection, connection:
            connection.execute("UPDATE catalog SET state_json = ?", (json.dumps(mixed),))
        try:
            run(config_path, bots_path, True, True, directory / "invalid-backup")
        except ValueError:
            pass
        else:
            raise AssertionError("malformed catalog applied")
        assert config_path.read_text() == config_text
        assert not (directory / "invalid-backup").exists()
        with closing(sqlite3.connect(bots_path)) as connection, connection:
            assert connection.execute("SELECT state_json FROM catalog").fetchone()[0] == json.dumps(mixed)
    print("self-test passed: synthetic config + bots only")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway-config", type=Path)
    parser.add_argument("--bots-db", type=Path)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--confirm-stopped", action="store_true")
    parser.add_argument("--backup-dir", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        if args.self_test:
            require(not (args.gateway_config or args.bots_db or args.apply), "self-test does not accept real paths/apply")
            self_test()
            return
        for path in (args.gateway_config, args.bots_db):
            require(path is None or path.is_absolute(), "input paths must be absolute")
        print(json.dumps(run(args.gateway_config, args.bots_db, args.apply,
                             args.confirm_stopped, args.backup_dir), indent=2))
    except (ValueError, KeyError, TypeError, OSError, sqlite3.Error) as error:
        # Decoder/SQLite errors may contain operator data; never echo their contents.
        message = str(error) if isinstance(error, UpgradeError) else type(error).__name__
        parser.exit(1, f"Upgrade refused or interrupted: {message}. Keep the gateway stopped; inspect verified backups before restarting.\n")


if __name__ == "__main__":
    main()
