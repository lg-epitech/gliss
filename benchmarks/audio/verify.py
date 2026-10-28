"""Verify the retained audio cohorts and print their scoped measurements."""

import gzip
import hashlib
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def require(condition, message):
    if not condition:
        raise ValueError(message)


def close(actual, expected):
    require(math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-7),
            f"Measurement mismatch: {actual} != {expected}")


def percentile(values, fraction):
    values = sorted(values)
    return values[min(len(values) - 1, math.ceil(len(values) * fraction) - 1)] if values else None


def span(rows, field):
    values = [field(row) for row in rows]
    return f"{min(values):.2f}–{max(values):.2f}"


def main():
    manifest = json.loads((HERE / "results.json").read_text())
    plan_bytes = (HERE / manifest["plan"]).read_bytes()
    plan = json.loads(plan_bytes)
    digest = lambda value: hashlib.sha256(value).hexdigest()
    expected = [(p["name"], mode) for p in plan["profiles"] for mode in plan["order"]]
    cohorts = []
    for record in manifest["runs"]:
        packed = (HERE / record["file"]).read_bytes()
        require(len(packed) == record["compressed_bytes"], "Compressed length changed")
        require(digest(packed) == record["compressed_sha256"], "Compressed artifact changed")
        raw = gzip.decompress(packed)
        require(len(raw) == record["original_bytes"] <= 1048576, "Original length changed")
        require(digest(raw) == record["original_sha256"], "Original JSON bytes changed")
        data = json.loads(raw)
        require(data["schema"] == "gliss-audio-performance-results-v1", "Unknown result schema")
        require(data["binary_sha256"] == record["binary_sha256"], "Binary binding changed")
        require(data["plan_sha256"] == digest(plan_bytes), "Plan binding changed")
        require(data["cleanup_complete"] is True, "Guest cleanup not confirmed")
        require(data["guest_cpu"] == 19 and data["native_browser_cpu"] == 0, "CPU placement changed")
        require(len(data["positions"]) == len(expected) == 12, "Positions missing")
        for source, pinned in data["source_pins"].items():
            require(digest((ROOT / source).read_bytes()) == pinned, f"Benchmark source changed: {source}")
        for slot, (position, treatment) in enumerate(zip(data["positions"], expected), 1):
            require(position["slot"] == slot, "Position order changed")
            require((position["profile"], position["mode"]) == treatment, "Treatment changed")
            require(position["status"] == "measured", "Position failed or blocked")
            metrics = position["metrics"]
            require(metrics["window_ms"] == plan["measurement_ms"], "Observation window changed")
            for kind, cpu in [("native", "0"), ("guest", "19")]:
                measured = metrics[kind + "_cpu"]
                require(measured["allowed_cpus"] == cpu, "Measured CPU placement changed")
                close(measured["percent_of_one_core"], measured["ticks"] / measured["ticks_per_second"]
                      * 100000 / measured["elapsed_ms"])
            browser = metrics["browser_cpu"]
            require(browser["complete_endpoint_set"] is True, "Browser endpoints incomplete")
            require(browser["matched_processes"] == browser["before_processes"] == browser["after_processes"],
                    "Browser process set changed")
            close(browser["percent_of_one_core"], browser["cpu_seconds"] * 100000 / browser["elapsed_ms"])
            wire = metrics["audio_wire"]
            close(metrics["audio_received_kbit_s"], wire["bytes"] * 8 / metrics["window_ms"])
            if wire["frames"]:
                close(metrics["audio_kbit_per_second_of_received_samples"],
                      wire["bytes"] * 8 * 48000 / wire["frames"] / 1000)
                require(position["mode"] != "off", "Muted position received audio")
            else:
                require(position["mode"] == "off" and wire["bytes"] == 0, "Enabled position received no audio")
            pings = metrics["pings"]
            require(len(pings) == 20, "Ping positions missing")
            replies = []
            for ping in pings:
                require(0 <= ping["sent_ms"] < metrics["window_ms"], "Ping sent outside window")
                if ping["received_ms"] is not None:
                    require(ping["sent_ms"] <= ping["received_ms"] < metrics["window_ms"],
                            "Reply outside observation window")
                    replies.append(ping["received_ms"] - ping["sent_ms"])
            summary = metrics["ping"]
            require(summary["sent"] == 20 and summary["completed"] == len(replies)
                    and summary["missed_at_cutoff"] == 20 - len(replies), "Ping counts changed")
            require(summary["complete"] == (len(replies) == 20), "Incomplete replies marked complete")
            close(summary["responded_p50_ms"], percentile(replies, 0.5))
            close(summary["responded_p95_ms"], percentile(replies, 0.95))
            close(summary["responded_max_ms"], max(replies))
            samples = metrics["samples"]
            require(len(samples) > 0, "Audio observations missing")
            require(all(0 <= row["at_ms"] < metrics["window_ms"] for row in samples), "Sample outside window")
            queues = [row["queuedSeconds"] * 1000 for row in samples]
            for key, fraction in [("p50", 0.5), ("p95", 0.95)]:
                close(metrics["playback_queue_ms"][key], percentile(queues, fraction))
            close(metrics["playback_queue_ms"]["max"], max(queues))
            if all("pendingDecodes" in row for row in samples):
                require(metrics["decoder_pending_max"] == max(row["pendingDecodes"] for row in samples),
                        "Pending-output summary changed")
            require(metrics["video_errors_delta"] == 0, "Video decode errors recorded")
            if position["profile"] == "limited":
                cleanup = position["proxy_cleanup"]
                require(cleanup["pendingTimers"] == cleanup["overflows"] == 0, "Proxy cleanup or overflow failure")
        cohorts.append(data)
        print(record["name"] + ": " + record["assessment"])
        for profile in plan["profiles"]:
            for mode in dict.fromkeys(plan["order"]):
                rows = [p["metrics"] for p in data["positions"] if p["profile"] == profile["name"] and p["mode"] == mode]
                completed = sum(row["ping"]["completed"] for row in rows)
                print(f"  {profile['name']:8} {mode:4}: native {span(rows, lambda r: r['native_cpu']['percent_of_one_core'])}%"
                      f", browser {span(rows, lambda r: r['browser_cpu']['percent_of_one_core'])}% of one core;"
                      f" received {span(rows, lambda r: r['audio_received_kbit_s'])} kbit/s;"
                      f" replies {completed}/40; conditional reply p95 {span(rows, lambda r: r['ping']['responded_p95_ms'])} ms;")
    require(all(data["source_pins"] == cohorts[0]["source_pins"] for data in cohorts), "Benchmark sources differ between cohorts")
    observation = json.loads((HERE / "timestamp-observation.json").read_text())
    require(digest((ROOT / observation["test"]).read_bytes()) == observation["test_sha256"], "Timestamp test changed")
    require(observation["result"]["outputs"] == 320, "Timestamp output count changed")
    for row in observation["result"]["rounded"]:
        require(row["actual"] - row["expected"] == row["delta"] == -1, "Timestamp diagnostic changed")
    stereo = json.loads((HERE / "stereo-observations.json").read_text())
    require(digest((ROOT / stereo["test"]).read_bytes()) == stereo["test_sha256"], "Stereo test changed")
    require([run["build"] for run in stereo["runs"]] == ["freshness-first", "freshness-final"],
            "Stereo smoke history changed")
    for run in stereo["runs"]:
        require([row["codec"] for row in run["observations"]] == ["opus", "pcm"], "Stereo codec positions changed")
        for row in run["observations"]:
            require(row["videoErrors"] == 0 and len(row["signal"]) == 2, "Stereo smoke observation changed")
            for channel, frequency in zip(row["signal"], [440, 880]):
                require(channel["rms"] > 0.01 and abs(channel["hz"] - frequency) < 40,
                        "Distinct stereo tones not observed")
            if run["build"] == "freshness-final":
                require(row["player"]["scheduled"] == row["player"]["received"] == row["wire"]["packets"] == 151
                        and row["player"]["dropped"] == 0, "Final stereo smoke dropped packets")
    print("Verified 36 retained positions, all original JSON hashes, source pins, rates, CPU arithmetic, queues and ping counts.")


if __name__ == "__main__":
    main()
