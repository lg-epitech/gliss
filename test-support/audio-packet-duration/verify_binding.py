#!/usr/bin/env python3
"""Verify test-support/audio-packet-duration against source-bindings.json.

Standard library only. Authored for root to run serially; it was not executed
by the author. Run from anywhere:

    python3 -I -B test-support/audio-packet-duration/verify_binding.py

Checks, all exact SHA-256 over raw bytes:
  * every parent source file still matches its frozen hash and length;
  * every owned file matches its frozen hash and length;
  * legacy.rs begins with the byte-identical original audio.rs (prefix hash
    equals the parent hash and the recorded prefix hash) and its appended
    suffix starts with the bridge marker and is test-scoped;
  * the directory holds only the listed files, at most `limits.max_files`;
  * the bindings file carries no hash of itself (no self-cycle).
Exit status 0 only if every check passes. Every actual hash is printed so
root can re-freeze deliberately after an intentional change (for example a
`cargo fmt` pass), never silently.
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> int:
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=here.parents[1],
                        help="repository root (default: two levels above this script)")
    parser.add_argument("--bindings", type=Path, default=here / "source-bindings.json")
    args = parser.parse_args()
    root: Path = args.root.resolve()
    bindings_path: Path = args.bindings.resolve()
    directory = bindings_path.parent

    failures: list[str] = []

    def check(condition: bool, message: str) -> None:
        if not condition:
            failures.append(message)

    try:
        bindings = json.loads(bindings_path.read_bytes())
    except (OSError, ValueError) as error:
        print(f"FAIL cannot read bindings: {error}")
        return 1

    check(bindings.get("schema") == "gliss-audio-packet-duration-source-bindings-v1",
          "unexpected bindings schema")
    self_entry = bindings.get("self", {})
    check(self_entry.get("file") == bindings_path.name, "self.file must name the bindings file")
    check(self_entry.get("sha256") is None, "self.sha256 must be null: no self-cycle")

    print("parents:")
    parent_hashes: dict[str, str] = {}
    for relative, expected in bindings.get("parents", {}).items():
        path = root / relative
        try:
            data = path.read_bytes()
        except OSError as error:
            failures.append(f"parent {relative}: {error}")
            continue
        actual = sha256(data)
        parent_hashes[relative] = actual
        status = "ok" if actual == expected["sha256"] and len(data) == expected["bytes"] else "DRIFT"
        print(f"  {status:5} {actual} {len(data):>8} {relative}")
        check(status == "ok", f"parent {relative} drifted from its frozen hash/length")

    print("owned:")
    owned = bindings.get("owned", {})
    for name, expected in owned.items():
        path = directory / name
        try:
            data = path.read_bytes()
        except OSError as error:
            failures.append(f"owned {name}: {error}")
            continue
        actual = sha256(data)
        status = "ok" if actual == expected["sha256"] and len(data) == expected["bytes"] else "DRIFT"
        print(f"  {status:5} {actual} {len(data):>8} {name}")
        check(status == "ok", f"owned {name} does not match its frozen hash/length")
        for parent in expected.get("derived_from", []):
            check(parent in bindings.get("parents", {}),
                  f"owned {name} derives from unlisted parent {parent}")
        prefix = expected.get("prefix")
        if prefix:
            parent = prefix["parent"]
            head = data[: prefix["bytes"]]
            head_hash = sha256(head)
            print(f"  {'ok' if head_hash == prefix['sha256'] else 'DRIFT':5} {head_hash} "
                  f"{len(head):>8} {name}[:{prefix['bytes']}] (prefix of {parent})")
            check(len(head) == prefix["bytes"], f"{name} is shorter than its recorded prefix")
            check(head_hash == prefix["sha256"], f"{name} prefix hash differs from the record")
            check(head_hash == bindings["parents"][parent]["sha256"],
                  f"{name} prefix is not byte-identical to the frozen {parent}")
            if parent in parent_hashes:
                check(head_hash == parent_hashes[parent],
                      f"{name} prefix is not byte-identical to the current {parent}")
            suffix = expected.get("suffix", {})
            tail = data[prefix["bytes"]:]
            marker = suffix.get("marker", "").encode()
            check(bool(marker) and tail.startswith(marker),
                  f"{name} suffix does not start with the bridge marker")
            check(b"#[cfg(test)]" in tail, f"{name} suffix is not test-scoped")
            tail_hash = sha256(tail)
            print(f"  {'ok' if tail_hash == suffix.get('sha256') else 'DRIFT':5} {tail_hash} "
                  f"{len(tail):>8} {name}[{prefix['bytes']}:] (bridge suffix)")
            check(tail_hash == suffix.get("sha256"), f"{name} suffix hash differs from the record")

    listed = set(owned) | {bindings_path.name}
    present = {entry.name for entry in directory.iterdir()}
    extra = sorted(present - listed)
    missing = sorted(listed - present)
    check(not extra, f"unlisted files present: {extra}")
    check(not missing, f"listed files missing: {missing}")
    max_files = bindings.get("limits", {}).get("max_files", 7)
    check(len(listed) <= max_files, f"{len(listed)} files exceed the limit of {max_files}")
    print(f"files: {len(present)} present, {len(listed)} listed, limit {max_files}")

    for message in failures:
        print(f"FAIL {message}")
    print("RESULT", "PASS" if not failures else f"FAIL ({len(failures)})")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
