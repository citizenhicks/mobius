#!/usr/bin/env python3
"""Synchronize native protocol constants and exact desktop crate requirements.

The gateway wire constant and package manifest are authoritative. Build scripts
use --check; maintainers run without it when updating the gateway contract.
"""
import argparse
from pathlib import Path
import re


def replace(path, pattern, value, check):
    original = path.read_text()
    updated, count = re.subn(pattern, lambda match: match[1] + value + match[2], original)
    if count != 1:
        raise SystemExit(f"Expected one contract declaration in {path}")
    if original != updated:
        if check:
            raise SystemExit(f"Gateway contract mismatch in {path}; run sync-gateway-contract.py")
        path.write_text(updated)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--swift", type=Path, action="append", default=[])
    parser.add_argument("--python-fixture", type=Path, action="append", default=[])
    parser.add_argument("--desktop-manifest", type=Path)
    args = parser.parse_args()
    core = Path(__file__).resolve().parents[1]
    wire = (core / "crates/mobius-gateway/src/wire.rs").read_text()
    protocol = re.search(r"PROTOCOL_VERSION: u16 = (\d+);", wire)[1]
    gateway = (core / "crates/mobius-gateway/Cargo.toml").read_text()
    gateway_version = re.search(r'^version = "([^"]+)"', gateway, re.M)[1]
    core_manifest = (core / "Cargo.toml").read_text()
    core_version = re.search(r'^version = "([^"]+)"', core_manifest, re.M)[1]
    for swift in args.swift:
        replace(swift, r"(let gatewayProtocolVersion = )\d+()", protocol, args.check)
    for fixture in args.python_fixture:
        replace(fixture, r"(?m)^(PROTOCOL_VERSION = )\d+()$", protocol, args.check)
    if args.desktop_manifest:
        for crate, version in (("mobius", core_version), ("mobius-gateway", gateway_version)):
            replace(args.desktop_manifest, rf'(?m)^({crate} = \{{ version = ")[^"]+(".*)$', "=" + version, args.check)
    cli = core / "crates/mobius-cli/Cargo.toml"
    replace(cli, r'(?m)^(mobius-gateway = \{ version = ")[^"]+(".*)$', "=" + gateway_version, args.check)
    print(f"Gateway contract: protocol {protocol}, core {core_version}, gateway {gateway_version}")


if __name__ == "__main__":
    main()
