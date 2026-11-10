#!/usr/bin/env python3
"""Reject duplicate Objective-C classes, including hidden classes, in two binaries."""
import argparse
import re
import subprocess
from pathlib import Path


def classes(binary: Path, architecture: str) -> set[str]:
    output = subprocess.check_output(
        ["xcrun", "otool", "-arch", architecture, "-ov", str(binary)], text=True
    )
    names = set()
    in_classes = False
    for line in output.splitlines():
        if line.startswith("Contents of "):
            in_classes = ",__objc_classlist)" in line
        # class_ro_t's name has eight spaces; methods/protocols nested inside a
        # class are indented farther. Export symbols omit Chromium's hidden types.
        if in_classes and (match := re.fullmatch(r"        name\s+0x[\da-fA-F]+ (\S+)", line)):
            names.add(match[1])
    if not names:
        raise ValueError(f"No Objective-C class definitions found in {binary} ({architecture})")
    return names


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("first", type=Path)
    parser.add_argument("second", type=Path)
    args = parser.parse_args()
    architectures = [
        set(subprocess.check_output(["xcrun", "lipo", "-archs", str(path)], text=True).split())
        for path in (args.first, args.second)
    ]
    shared = architectures[0] & architectures[1]
    if not shared:
        raise SystemExit("Frameworks have no shared architecture")
    for architecture in sorted(shared):
        first, second = classes(args.first, architecture), classes(args.second, architecture)
        overlap = first & second
        if overlap:
            raise SystemExit(f"Duplicate Objective-C classes ({architecture}): {', '.join(sorted(overlap))}")
        print(f"Objective-C classes ({architecture}): {len(first)} + {len(second)}, no overlap")


if __name__ == "__main__":
    main()
