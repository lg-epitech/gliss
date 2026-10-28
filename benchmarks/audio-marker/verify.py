"""Recompute source-marker and mixer observations without a VM."""
import gzip
import hashlib
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def require(value, message):
    if not value:
        raise ValueError(message)


def main():
    digest = lambda raw: hashlib.sha256(raw).hexdigest()
    manifest = json.loads((HERE / "results.json").read_text())
    binding = manifest["mixer"]
    plan_bytes = (HERE / binding["plan"]).read_bytes()
    require(digest(plan_bytes) == binding["plan_sha256"], "Mixer plan changed")
    plan = json.loads(plan_bytes)
    for name, expected in plan["source_pins"].items():
        require(digest((ROOT / name).read_bytes()) == expected, f"Source changed: {name}")
    packed = (HERE / binding["file"]).read_bytes()
    require(digest(packed) == binding["compressed_sha256"], "Compressed observation changed")
    raw = gzip.decompress(packed)
    require(digest(raw) == binding["original_sha256"], "Observation bytes changed")
    data = json.loads(raw)
    require(len(data["runs"]) == len(plan["order"]) == 4, "Mixer positions missing")
    count = 0
    for index, (run, frequency) in enumerate(zip(data["runs"], plan["order"])):
        require(run["round"] == index and run["mix_frequency"] == frequency, "Mixer order changed")
        log = run["original_log"]
        require(digest(log.encode()) == run["original_log_sha256"], "Original log changed")
        rows = [json.loads(line[2:]) for line in log.splitlines() if line.startswith("ℹ {")]
        require([row["codec"] for row in rows] == plan["codecs"], "Codec positions missing")
        require("ℹ pass 1\n" in log and "ℹ fail 0\n" in log, "Component failed")
        for row in rows:
            require(row["mix_frequency"] == frequency and f"out.frequency={frequency}," in row["audio_device"],
                    "Mixer invocation changed")
            require([pulse["code"] for pulse in row["pulses"]] == plan["codes"], "Marker positions changed")
            state = row["state"]
            require(state["probe_failure"] is None and state["video_errors"] == 0, "Observer or video failure")
            require(state["quality_windows_dropped"] == 0, "Quality observations truncated")
            for pulse in row["pulses"]:
                require(pulse["token"] == pulse["source_note"]["token"], "Source token mismatch")
                require(pulse["code"] == pulse["source_note"]["code"] == pulse["observation"]["code"], "Source code mismatch")
                measured = pulse["observation"]["host_received_ms"] - pulse["submitted_host_ms"]
                require(math.isclose(measured, pulse["trigger_to_observed_ms"], abs_tol=1e-7), "Latency arithmetic mismatch")
                require(0 <= measured < plan["per_marker_deadline_ms"], "Marker missed its deadline")
                count += 1
            steady = [window for window in state["quality_windows"] if window["code"] == 1
                      and all(channel["concentration"] > 0.95 for channel in window["quality"])]
            require(len(steady) > 0, "Steady code-1 windows missing")
            amplitude = max(window["quality"][1]["amplitude"] for window in steady)
            ratio = amplitude / plan["expected_amplitude"][1]
            maximum = max(pulse["trigger_to_observed_ms"] for pulse in row["pulses"])
            print(f"round {index}, mixer {frequency}, {row['codec']}: 6/6 markers; "
                  f"11 kHz amplitude {amplitude:.6f}, source ratio {ratio:.5f}, "
                  f"response max {maximum:.1f} ms")
    require(count == 48, "Marker results missing")
    print("Verified all four mixer positions, eight codec sessions and 48 marker responses.")


if __name__ == "__main__":
    main()
