"""Check the complete matched TCP-policy measurements without starting a VM."""
import gzip
import hashlib
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def require(value, reason):
    if not value:
        raise ValueError(reason)


def close(actual, expected):
    require(math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-7), "Arithmetic mismatch")


def percentile(values, fraction):
    values = sorted(values)
    return values[min(len(values) - 1, math.ceil(len(values) * fraction) - 1)]


def main():
    manifest = json.loads((HERE / "results.json").read_text())
    for record in manifest["runs"]:
        packed = (HERE / record["file"]).read_bytes()
        sha = lambda raw: hashlib.sha256(raw).hexdigest()
        require(sha(packed) == record["compressed_sha256"], "Compressed evidence changed")
        raw = gzip.decompress(packed)
        require(len(raw) <= 1048576 and sha(raw) == record["original_sha256"], "Original evidence changed")
        data = json.loads(raw)
        plan_bytes = (HERE / record["plan"]).read_bytes()
        plan = json.loads(plan_bytes)
        require(sha(plan_bytes) == data["plan_sha256"], "Plan changed")
        require(data["binary_sha256"] == record["binary_sha256"], "Binary binding changed")
        require(data["cleanup_complete"] is True, "Cleanup unconfirmed")
        require(data["native_browser_cpu"] == 0 and data["guest_cpu"] == 19, "CPU placement changed")
        for source, expected in data["source_pins"].items():
            require(sha((ROOT / source).read_bytes()) == expected, f"Source changed: {source}")
        expected = [(profile["name"], treatment) for profile in plan["profiles"] for treatment in plan["order"]]
        require(len(data["positions"]) == len(expected) == 24, "Positions missing")
        for slot, (position, (profile, treatment)) in enumerate(zip(data["positions"], expected), 1):
            require(position["slot"] == slot and position["status"] == "measured", "Position failed or moved")
            require(position["profile"] == profile and all(position[key] == value for key, value in treatment.items()),
                    "Treatment changed")
            policy = position["socket_policy"]
            requested = position["tcp_nodelay"]
            require(policy["requested_nodelay"] is requested, "TCP policy mismatch")
            count = policy["accepted_sockets"]
            require(count > 0 and policy["verified_sockets"] == count, "Socket readback incomplete")
            require(policy["initial_true"] + policy["initial_false"] == count, "Initial socket counts mismatch")
            require(policy["verified_readback_true"] == (count if requested else 0)
                    and policy["verified_readback_false"] == (0 if requested else count), "Socket readback mismatch")
            require(all(policy[key] == 0 for key in ["rejected_sockets", "initial_read_errors", "set_errors",
                                                   "readback_errors", "readback_mismatches"]), "Socket policy failure")
            metrics = position["metrics"]
            require(metrics["window_ms"] == plan["measurement_ms"], "Measurement window changed")
            require(metrics["video_errors_delta"] == 0, "Video decode error")
            replies = []
            for ping in metrics["pings"]:
                require(0 <= ping["sent_ms"] < metrics["window_ms"], "Ping outside window")
                if ping["received_ms"] is not None:
                    require(ping["sent_ms"] <= ping["received_ms"] < metrics["window_ms"], "Reply outside window")
                    replies.append(ping["received_ms"] - ping["sent_ms"])
            ping = metrics["ping"]
            require(len(metrics["pings"]) == ping["sent"] == 20, "Ping positions missing")
            require(ping["completed"] == len(replies) and ping["missed_at_cutoff"] == 20 - len(replies), "Reply count mismatch")
            require(ping["complete"] == (len(replies) == 20), "Missing replies marked complete")
            close(ping["responded_p50_ms"], percentile(replies, 0.5))
            close(ping["responded_p95_ms"], percentile(replies, 0.95))
            close(ping["responded_max_ms"], max(replies))
            close(metrics["audio_received_kbit_s"], metrics["audio_wire"]["bytes"] * 8 / metrics["window_ms"])
            for key in ["native_cpu", "guest_cpu"]:
                cpu = metrics[key]
                close(cpu["percent_of_one_core"], cpu["ticks"] / cpu["ticks_per_second"] * 100000 / cpu["elapsed_ms"])
            cpu = metrics["browser_cpu"]
            require(cpu["complete_endpoint_set"] is True, "Browser endpoints incomplete")
            close(cpu["percent_of_one_core"], cpu["cpu_seconds"] * 100000 / cpu["elapsed_ms"])
            queues = [row["queuedSeconds"] * 1000 for row in metrics["samples"]]
            close(metrics["playback_queue_ms"]["p95"], percentile(queues, 0.95))
            close(metrics["playback_queue_ms"]["max"], max(queues))
            if metrics.get("proxy") is not None:
                require(position["proxy_cleanup"]["pendingTimers"] == position["proxy_cleanup"]["overflows"] == 0,
                        "Proxy cleanup or overflow failure")
        print(record["name"] + ": 24 original positions verified; " + record["qualification"])
        for profile in plan["profiles"]:
            for mode in ["off", "opus", "pcm"]:
                for requested in [False, True]:
                    rows = [p["metrics"] for p in data["positions"] if p["profile"] == profile["name"]
                            and p["mode"] == mode and p["tcp_nodelay"] is requested]
                    p95 = [row["ping"]["responded_p95_ms"] for row in rows]
                    require(len(rows) == 2, "Repetition missing")
                    print(f"  {profile['name']} {mode} nodelay={requested}: "
                          f"{sum(row['ping']['completed'] for row in rows)}/40 replies, "
                          f"conditional reply p95 {min(p95):.1f}–{max(p95):.1f} ms")


if __name__ == "__main__":
    main()
