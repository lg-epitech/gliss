"""Exact pre-frame-skip runtime sources used by the retained audio cohorts."""
import gzip
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
raw = (ROOT / "benchmarks/audio-fallback/runtime-sources.json.gz").read_bytes()
if hashlib.sha256(raw).hexdigest() != "51de43b5e1485cda7d6f4a57946c40885b3c1db55f16728604181bd976b59ca8":
    raise ValueError("Runtime source snapshot changed")
snapshot = json.loads(gzip.decompress(raw))
if snapshot["schema"] != "gliss-runtime-source-snapshot-v1" or snapshot["source_commit"] != "9b4de093":
    raise ValueError("Unknown runtime snapshot")


def runtime_source_bytes(name):
    if name in snapshot["sources"]:
        return snapshot["sources"][name].encode()
    return (ROOT / name).read_bytes()
