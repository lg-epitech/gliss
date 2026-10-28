"""Read exact historical audio runners after their explicit-PCM update."""
import gzip
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKED = ROOT / "benchmarks/audio/retained-sources.json.gz"
EXPECTED = "e338d31d9ed5a02e50c8178540540245bfd02b7244c7315ca0c18569ca0f1e7f"
raw = PACKED.read_bytes()
if hashlib.sha256(raw).hexdigest() != EXPECTED:
    raise ValueError("Retained source snapshot changed")
snapshot = json.loads(gzip.decompress(raw))
if snapshot["schema"] != "gliss-audio-retained-source-snapshot-v1" or snapshot["source_commit"] != "639aa4b2":
    raise ValueError("Unknown source snapshot")


def source_bytes(name):
    if name in snapshot["sources"]:
        return snapshot["sources"][name].encode()
    return (ROOT / name).read_bytes()
