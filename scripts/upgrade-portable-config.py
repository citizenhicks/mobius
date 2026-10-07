#!/usr/bin/env python3
"""Offline gateway config upgrade; never run on a live gateway.

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
SECTION = re.compile(r"^\s*\[([^\[\]]+)\]\s*(?:#.*)?$")
ASSIGNMENT = re.compile(r"^(\s*)([a-zA-Z0-9_]+)(\s*=\s*)(.*)$")
TOML_COMMENT = re.compile(r'''(?:"(?:[^"\\]|\\.)*"|'[^']*')|(#.*)''')
MODEL_SOURCE = Path(__file__).resolve().parents[1] / "src/backend/model"


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


def validate_catalog(values):
    require(isinstance(values, list) and len(values) <= 64, "provider catalogs allow at most 64 entries")
    total_bytes = 0
    for value in values:
        require(isinstance(value, str) and value and value == value.strip() and
                not re.search(r"[\x00-\x1f\x7f-\x9f]", value), "invalid provider catalog entries")
        size = len(value.encode("utf-8"))
        require(size <= 1024, "provider catalog entries allow at most 1024 UTF-8 bytes")
        total_bytes += size
    require(total_bytes <= 16 * 1024, "provider catalogs allow at most 16 KiB of UTF-8 entries")
    require(len(set(values)) == len(values), "duplicate provider catalog entries")


def validate_models(models):
    require(isinstance(models, list), "invalid configured models")
    required = {"id", "label", "description", "context_window", "reasoning", "tool_discovery"}
    for model in models:
        require(isinstance(model, dict) and required <= set(model) <= required | {"default_reasoning"} and
                isinstance(model["label"], str) and model["label"].strip() and len(model["label"].encode()) <= 1024 and
                isinstance(model["description"], str) and len(model["description"].encode()) <= 16 * 1024 and
                type(model["context_window"]) is int and 0 < model["context_window"] < 2**63 and
                model["tool_discovery"] in ("native", "rebuild"), "invalid configured model metadata")
        choices = model["reasoning"]
        require(isinstance(choices, list) and all(isinstance(choice, dict) and {"id", "label"} <= set(choice) <= {
                "id", "label", "description"} and isinstance(choice["label"], str) and choice["label"].strip() and
                len(choice["label"].encode()) <= 1024 and isinstance(choice.get("description", ""), str) and
                len(choice.get("description", "").encode()) <= 16 * 1024 for choice in choices), "invalid model reasoning choices")
        efforts = [choice["id"] for choice in choices]
        validate_catalog(efforts)
        default = model.get("default_reasoning")
        require(default in efforts if efforts else default is None,
                "model default_reasoning must select a listed effort, or be absent when none exist")
    validate_catalog([model["id"] for model in models])


def provider_model_catalog(selection, models_dir):
    provider_id = selection.get("provider") if isinstance(selection, dict) else None
    require(isinstance(provider_id, str) and re.fullmatch(r"[a-z][a-z0-9_]*", provider_id),
            "missing or invalid provider ID for catalog seeding")
    owner = "openai" if provider_id == "responses" else provider_id
    manifest_path = MODEL_SOURCE / f"{owner}_provider.toml"
    require(manifest_path.is_file(), "provider source manifest is unavailable")
    manifest = tomllib.loads(manifest_path.read_text())
    if manifest.get("locked_models", False):
        return manifest, {"models": []}
    path = MODEL_SOURCE / f"{provider_id}.toml"
    if not path.is_file():
        return manifest, {"models": []}
    override = models_dir / path.name if models_dir else None
    if override is not None and (override.exists() or override.is_symlink()):
        require(override.is_file() and not override.is_symlink(), "model catalog override must be a regular file")
        path = override
    try:
        catalog = tomllib.loads(path.read_text())
        require(set(catalog) <= {"default_model", "models", "image_models"} and
                isinstance(catalog.get("models"), list), "invalid provider model catalog")
        models = catalog["models"]
        validate_models(models)
        require(catalog.get("default_model") is None or catalog["default_model"] in
                [model["id"] for model in models], "invalid provider catalog default model")
        images = catalog.get("image_models", [])
        require(isinstance(images, list), "invalid provider image catalog")
        image_ids = [model["id"] for model in images]
        require(all(isinstance(value, str) and value.strip() for value in image_ids) and
                len(set(image_ids)) == len(image_ids), "invalid provider image catalog IDs")
        for model in images:
            require(isinstance(model, dict) and {"id", "label", "description"} <= set(model) <= {
                    "id", "label", "description", "variants"} and isinstance(model["label"], str) and
                    isinstance(model["description"], str), "invalid provider image catalog model")
        for choices in [model.get("variants", []) for model in images]:
            require(isinstance(choices, list), "invalid provider catalog choices")
            choice_ids = [choice["id"] for choice in choices]
            require(all(isinstance(value, str) and value.strip() for value in choice_ids) and
                    len(set(choice_ids)) == len(choice_ids), "invalid provider catalog choice IDs")
            require(all(isinstance(choice, dict) and {"id", "label"} <= set(choice) <= {
                    "id", "label", "description"} and isinstance(choice["label"], str) and
                    isinstance(choice.get("description", ""), str) for choice in choices),
                    "invalid provider catalog choice")
        return manifest, catalog
    except (ValueError, KeyError, TypeError, OSError):
        raise UpgradeError("invalid provider model catalog or override; no fallback is used") from None


def expand_legacy_models(model_ids, efforts, selection, models_dir):
    manifest, catalog = provider_model_catalog(selection, models_dir)
    require(not manifest.get("locked_models", False) or not model_ids,
            "locked providers cannot contain custom model IDs")
    presets = {model["id"]: model for model in catalog["models"]}
    if not model_ids:
        return copy.deepcopy(catalog["models"])
    default = presets.get(catalog.get("default_model"))
    if default is not None:
        context_window = default["context_window"]
    else:
        config_source = (MODEL_SOURCE.parents[2] / "crates/mobius-gateway/src/config.rs").read_text()
        context = re.search(r"pub const DEFAULT_CONTEXT_WINDOW: i64 = ([0-9_]+);", config_source)
        require(context is not None, "gateway default context window is unavailable")
        context_window = int(context[1].replace("_", ""))
    models = []
    for model_id in model_ids:
        model = copy.deepcopy(presets.get(model_id, dict(id=model_id, label=model_id, description="",
            context_window=context_window, reasoning=[], tool_discovery=manifest["tool_discovery"])))
        choices = {choice["id"]: choice for choice in model["reasoning"]}
        model["reasoning"] = [copy.deepcopy(choices.get(effort, dict(id=effort, label=effort, description=""))) for effort in efforts]
        model.pop("default_reasoning", None)
        if efforts:
            model["default_reasoning"] = efforts[0]
        models.append(model)
    return models


def migrate_provider_models(providers, version, models_dir=None):
    require(isinstance(providers, dict), "invalid configured_providers table")
    routes = set()
    for instance, provider in providers.items():
        catalog_fields = {"models"} if version == 28 else {"model_ids", "reasoning_efforts"}
        require(isinstance(provider, dict) and set(provider) <= catalog_fields | {
            "selection", "label", "tint", "image_model_ids",
        }, "unknown or malformed configured provider")
        if version != 28:
            model_ids, efforts = provider.get("model_ids"), provider.get("reasoning_efforts")
            validate_catalog(model_ids)
            validate_catalog(efforts)
            require(model_ids or not efforts, "reasoning efforts have no listed model IDs")
            provider["models"] = expand_legacy_models(model_ids, efforts, provider.get("selection"), models_dir)
            del provider["model_ids"], provider["reasoning_efforts"]
        models = provider.get("models")
        validate_models(models)
        for model in models:
            efforts = [choice["id"] for choice in model["reasoning"]]
            for effort in efforts or ["default"]:
                route = f"{instance}::{model['id']}::{effort}"
                require(route not in routes, "configured models generate an ambiguous route")
                routes.add(route)
                require(len(routes) <= 64, "configured models allow at most 64 total routes")
        if models:
            selection = provider.get("selection")
            require(isinstance(instance, str) and re.fullmatch(r"[a-zA-Z0-9_.-]{1,256}", instance) and
                    isinstance(selection, dict) and selection.get("instance") == instance,
                    "invalid configured provider selection instance")
            selected = next((model for model in models if model["id"] == selection.get("model")), None)
            require(selected is not None, "selected model must be in the configured model catalog")
            effort = selection.get("reasoning_effort")
            require(effort is None or effort in [choice["id"] for choice in selected["reasoning"]],
                    "selected reasoning effort must be in the selected model's catalog")
        selection = provider.get("selection", {})
        require(isinstance(selection, dict) and selection.get("tool_discovery") in (None, "native", "rebuild"),
                "provider tool_discovery must be native, rebuild or absent")


def migrate_config(config, models_dir=None):
    """Mutate the parsed gateway config; return whether this one-off changed it."""
    require(isinstance(config, dict) and type(config.get("version")) is int and config["version"] in (26, 27, 28),
            "expected gateway config version 26, 27 or 28")
    require(set(config) <= {"version", "runtime", "connections", "auth", "computer",
            "model_transport", "execution", "telemetry", "listen", "tls", "cloudflare",
            "desktop_enabled", "bot_defaults", "configured_providers", "installed_extensions", "usage"},
            "unknown gateway configuration fields")
    version = config["version"]
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
    if version != 26:
        require(not changed, f"version {version} still contains retired settings")
    migrate_provider_models(config.get("configured_providers", {}), version, models_dir)
    if version == 28:
        return False
    config["version"] = 28
    return True


def parse_toml_assignment(lines, index):
    end = index
    while True:
        try:
            return tomllib.loads("".join(lines[index:end + 1])), end + 1
        except tomllib.TOMLDecodeError:
            end += 1
            require(end < len(lines), "unsupported TOML assignment formatting; no files changed")


def rewrite_provider_catalog(lines, index, models):
    parsed, end = parse_toml_assignment(lines, index)
    require(len(parsed) == 1 and set(parsed) <= {"model_ids", "reasoning_efforts"} and
            isinstance(next(iter(parsed.values())), list), "unsupported provider catalog formatting; no files changed")
    indent = lines[index][:len(lines[index]) - len(lines[index].lstrip())]
    output = ""
    if "model_ids" in parsed:
        output = indent + "models = " + toml_inline(models) + "\n"
    for line in lines[index:end]:
        for match in TOML_COMMENT.finditer(line):
            if match[1]:
                output += indent + match[1] + "\n"
    return output, end


def toml_inline(value):
    if isinstance(value, dict):
        return "{" + ", ".join(f"{key} = {toml_inline(item)}" for key, item in value.items()) + "}"
    if isinstance(value, list):
        return "[" + ", ".join(toml_inline(item) for item in value) + "]"
    return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007f")


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
            if value.lstrip().startswith(('"""', "'''")):
                _, end = parse_toml_assignment(lines, index)
                output.extend(lines[index:end])
                index = end
                continue
            if not section and name == "version":
                comment = " #" + value.partition("#")[2] if "#" in value else ""
                output.append(indent + "version" + equals + str(expected["version"]) + comment + "\n")
                index += 1
                continue
            if section.startswith("configured_providers.") and name in {"model_ids", "reasoning_efforts"}:
                providers = tomllib.loads(f"[{section}]")["configured_providers"]
                require(len(providers) == 1 and next(iter(providers.values())) == {},
                        "unsupported configured provider table; no files changed")
                models = expected["configured_providers"][next(iter(providers))]["models"]
                replacement, index = rewrite_provider_catalog(lines, index, models)
                output.append(replacement)
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
        if migrate_config(expected, config_path.parent / "models"):
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
    def custom_model(model_id, efforts=(), default=None, **metadata):
        model = dict(id=model_id, label=model_id, description="", context_window=272000,
                     tool_discovery="rebuild", reasoning=[dict(id=effort, label=effort, description="") for effort in efforts])
        if default is not None:
            model["default_reasoning"] = default
        model.update(metadata)
        return model

    providers_text = '''[configured_providers.local-llm]
label = "Synthetic endpoint"
model_ids = ["vendor/model-a", "vendor/model-b", "odd#]🚀"]
reasoning_efforts = [
  "high", # preserve order
  "off#]", # preserve literal punctuation
]
[configured_providers.local-llm.selection]
instance = "local-llm"
provider = "responses"
model = "vendor/model-a"
reasoning_effort = "high"
[configured_providers."openai.main"]
model_ids = []
reasoning_efforts = []
[configured_providers."openai.main".selection]
instance = "openai.main"
provider = "openai_socket"
model = "gpt-6-luna"
[configured_providers.no-reasoning]
model_ids = ["plain"]
reasoning_efforts = []
[configured_providers.no-reasoning.selection]
instance = "no-reasoning"
provider = "responses"
model = "plain"
'''
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
''' + providers_text
    config_text = config_text.replace('system_prompt = "Keep this text exactly # unchanged"',
                                     'system_prompt = """Keep this text exactly # unchanged\n'
                                     '[configured_providers.local-llm]\n'
                                     'reasoning_efforts = ["prompt example"]\n"""')
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
        assert upgraded["bot_defaults"]["config"]["system_prompt"] == automatic["config"]["system_prompt"]
        assert upgraded["bot_defaults"]["config"]["middleware"]["settings"]["compaction"] == {
            "allow_model_compaction": "off", "at_tokens": 250000, "reserve_tokens": 16384,
        }
        assert "# keep comment" in config_path.read_text() and "# old retry" in config_path.read_text()
        assert upgraded["version"] == 28
        assert upgraded["model_transport"] == {"stream_retry_limit": 5}
        provider = upgraded["configured_providers"]["local-llm"]
        assert provider["models"] == [
            custom_model(model, ["high", "off#]"], "high")
            for model in ["vendor/model-a", "vendor/model-b", "odd#]🚀"]
        ]
        assert not {"model_ids", "reasoning_efforts"} & provider.keys()
        assert provider["selection"] == {"instance": "local-llm", "provider": "responses", "model": "vendor/model-a", "reasoning_effort": "high"}
        assert upgraded["configured_providers"]["openai.main"]["models"] == []
        assert upgraded["configured_providers"]["no-reasoning"]["models"] == [custom_model("plain")]
        assert "# preserve order" in config_path.read_text()
        assert "# preserve literal punctuation" in config_path.read_text()
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
        # Version 27 needs only the model-specific catalog conversion.
        config_path.write_text("version = 27\n" + providers_text)
        before = config_path.read_bytes()
        assert run(config_path, None)["config_changed"]
        assert config_path.read_bytes() == before
        run(config_path, None, True, True, directory / "version-27-backup")
        current = tomllib.loads(config_path.read_text())
        assert current["version"] == 28
        assert current["configured_providers"] == upgraded["configured_providers"]
        assert not run(config_path, None, True, True, directory / "unused-27-backup")["config_changed"]
        assert not (directory / "unused-27-backup").exists()
        # Editable presets retain owner catalogs, including defaults that are not first.
        for provider_id in ("anthropic", "kimi", "deepseek"):
            catalog = tomllib.loads((MODEL_SOURCE / f"{provider_id}.toml").read_text())
            expected_models = catalog["models"]
            for version in (26, 27):
                fixture = dict(version=version, configured_providers={"native": dict(model_ids=[], reasoning_efforts=[],
                    selection=dict(instance="native", provider=provider_id, model=catalog["default_model"]))})
                assert migrate_config(fixture)
                assert fixture["configured_providers"]["native"]["models"] == expected_models
                preserved = copy.deepcopy(fixture)
                assert not migrate_config(fixture) and fixture == preserved
        for provider_id in ("openai_socket", "openai_codex", "openrouter", "responses"):
            assert provider_model_catalog(dict(provider=provider_id), None)[1]["models"] == []
        try:
            provider_model_catalog(dict(provider="unknown_fixture"), None)
        except UpgradeError:
            pass
        else:
            raise AssertionError("unknown provider manifest accepted")
        models_dir = directory / "models"
        models_dir.mkdir()
        override_text = '''default_model = "fixture-native"
[[models]]
id = "fixture-native"
label = "Fixture"
description = "Synthetic override"
context_window = 200000
tool_discovery = "native"
reasoning = [{id = "low", label = "Low"}, {id = "high", label = "High"}]
default_reasoning = "high"
'''
        override_path = models_dir / "anthropic.toml"
        override_path.write_text(override_text)
        expanded = expand_legacy_models(["fixture-native", "other"], ["high", "custom"], dict(provider="anthropic"), models_dir)
        assert expanded[0] == dict(id="fixture-native", label="Fixture", description="Synthetic override", context_window=200000,
            tool_discovery="native", reasoning=[dict(id="high", label="High"), dict(id="custom", label="custom", description="")], default_reasoning="high")
        assert expanded[1] == custom_model("other", ["high", "custom"], "high", context_window=200000)
        override_path.write_text(override_text.replace('default_model = "fixture-native"\n', ""))
        assert expand_legacy_models(["other"], [], dict(provider="anthropic"), models_dir) == [custom_model("other")]
        override_path.write_text(override_text)
        config_path.write_text('''version = 27
[configured_providers.native]
model_ids = []
reasoning_efforts = []
[configured_providers.native.selection]
instance = "native"
provider = "anthropic"
model = "fixture-native"
reasoning_effort = "low"
''')
        original = config_path.read_bytes()
        assert run(config_path, None)["config_changed"] and config_path.read_bytes() == original
        run(config_path, None, True, True, directory / "override-backup")
        upgraded_override = tomllib.loads(config_path.read_text())["configured_providers"]["native"]
        assert upgraded_override["models"] == tomllib.loads(override_text)["models"]
        assert upgraded_override["selection"]["reasoning_effort"] == "low"
        assert override_path.read_text() == override_text
        media = [f'{{id = "image-{index}", label = "", description = ""}}' for index in range(65)]
        variants = ", ".join(f'{{id = "variant-{index}", label = ""}}' for index in range(65))
        media[0] = media[0][:-1] + ", variants = [" + variants + "]}"
        override_path.write_text("image_models = [" + ", ".join(media) + "]\n" + override_text)
        assert provider_model_catalog(dict(provider="anthropic"), models_dir)[1]["models"] == upgraded_override["models"]
        metadata_controls = custom_model("unicode", description="Escaped DEL: \x7f; emoji: 🚀")
        assert tomllib.loads("models = " + toml_inline([metadata_controls]))["models"] == [metadata_controls]
        for invalid_override in ("not TOML", override_text.replace('label = "Fixture"', 'label = 1'),
                                 override_text.replace('default_reasoning = "high"', 'default_reasoning = "missing"')):
            override_path.write_text(invalid_override)
            config_path.write_bytes(original)
            try:
                run(config_path, None, True, True, directory / "invalid-override-backup")
            except UpgradeError:
                pass
            else:
                raise AssertionError("invalid model override accepted")
            assert config_path.read_bytes() == original and not (directory / "invalid-override-backup").exists()
        override_path.unlink()
        # Current full model records retain metadata and explicit defaults byte for byte.
        config_path.write_text('''version = 28
[configured_providers.custom]
models = [
  { id = "a", label = "A", description = "Plain model", context_window = 100000, reasoning = [], tool_discovery = "native" },
  { id = "b", label = "B", description = "Other plain model", context_window = 100001, reasoning = [], tool_discovery = "rebuild" },
  { id = "c", label = "C", description = "Custom reasoning model", context_window = 100002, reasoning = [{id = "high", label = "Thorough"}, {id = "low", label = "Quick", description = "Fast mode"}], default_reasoning = "low", tool_discovery = "native" },
]
[configured_providers.custom.selection]
instance = "custom"
provider = "responses"
model = "c"
reasoning_effort = "high"
tool_discovery = "rebuild"
''')
        before = config_path.read_bytes()
        assert not run(config_path, None, True, True, directory / "unused-28-backup")["config_changed"]
        assert config_path.read_bytes() == before and not (directory / "unused-28-backup").exists()
        for version, provider in [
                (27, dict(model_ids=["a"], reasoning_efforts={"a": ["high"]})),
                (27, dict(model_ids=["a", "a"], reasoning_efforts=[])),
                (27, dict(model_ids=[], reasoning_efforts=["high"])),
                (27, dict(model_ids=["a"], reasoning_efforts=["high", "high"])),
                (27, dict(models=[])),
                (28, dict(model_ids=["a"], reasoning_efforts={"a": ["high"]})),
                (28, dict(models=[], model_ids=[])),
                (28, dict(models=[dict(id="a", reasoning_efforts=["high"], default_reasoning="high")])),
                (28, dict(models=[custom_model("a"), custom_model("a")])),
                (28, dict(models=[custom_model("a", extra=True)])),
                (28, dict(models=[custom_model("a", reasoning="high")])),
                (28, dict(models=[custom_model("a", ["high", "high"], "high")])),
                (28, dict(models=[custom_model("a", [" high"], " high")])),
                (28, dict(models=[custom_model("a", ["high"])])),
                (28, dict(models=[custom_model("a", ["high"], "low")])),
                (28, dict(models=[custom_model("a", default="high")])),
                (28, dict(models=[custom_model("a", context_window=0)])),
                (28, dict(models=[custom_model("a", label=" ")])),
                (28, dict(models=[custom_model("a", description="é" * 8193)])),
                (28, dict(models=[custom_model("a", ["high"], "high", reasoning=[dict(id="high", label="")])])),
                (28, dict(models=[custom_model("a", tool_discovery="invalid")]))]:
            provider["selection"] = dict(instance="custom", provider="responses", model="a")
            fixture = dict(version=version, configured_providers={"custom": provider})
            try:
                migrate_config(fixture)
            except UpgradeError:
                pass
            else:
                raise AssertionError("invalid configured model catalog accepted")
        # Catalog limits count UTF-8 bytes; model routes share one gateway-wide budget.
        full_bytes = [f"{index:02}" + "x" * 1022 for index in range(16)]
        for values in ([str(index) for index in range(64)], ["é" * 512], full_bytes):
            validate_catalog(values)
        for values in ([str(index) for index in range(65)], ["é" * 513], full_bytes + ["extra"]):
            try:
                validate_catalog(values)
            except UpgradeError:
                pass
            else:
                raise AssertionError("oversized provider catalog accepted")
        models = [custom_model(str(index)) for index in range(64)]
        provider = dict(models=models, selection=dict(instance="custom", model="0"))
        migrate_provider_models({"custom": provider}, 28)
        collision = [custom_model("a", ["b::high"], "b::high"), custom_model("a::b", ["high"], "high")]
        invalid_providers = [
            {"custom": dict(models=collision, selection=dict(instance="custom", model="a"))},
            {instance: dict(models=models[:33], selection=dict(instance=instance, model="0"))
             for instance in ("one", "two")},
            {"custom": dict(models=[custom_model("a")], selection=dict(instance="custom", model="missing"))},
            {"custom": dict(models=[custom_model("a")], selection=dict(instance="custom", model="a", reasoning_effort="high"))},
            {"custom": dict(models=collision[:1], selection=dict(instance="custom", model="a", reasoning_effort="high"))},
        ]
        for providers in invalid_providers:
            try:
                migrate_provider_models(providers, 28)
            except UpgradeError:
                pass
            else:
                raise AssertionError("invalid configured model routes or selection accepted")
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
