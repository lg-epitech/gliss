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
    if "latency" in manifest:
        verify_latency(manifest, digest)


def verify_latency(manifest, digest):
    binding = manifest["latency"]
    plan_bytes = (HERE / binding["plan"]).read_bytes()
    require(digest(plan_bytes) == binding["plan_sha256"], "Latency plan changed")
    plan = json.loads(plan_bytes)
    packed = (HERE / binding["file"]).read_bytes()
    require(digest(packed) == binding["compressed_sha256"], "Compressed latency evidence changed")
    raw = gzip.decompress(packed)
    require(digest(raw) == binding["original_sha256"], "Original latency evidence changed")
    data = json.loads(raw)
    require(data["plan_sha256"] == binding["plan_sha256"] and data["binary_sha256"] == manifest["binary_sha256"],
            "Latency plan/binary binding mismatch")
    require(data["cleanup_complete"] is True, "Guest cleanup unconfirmed")
    for name, expected in data["source_pins"].items():
        require(digest((ROOT / name).read_bytes()) == expected, f"Latency source changed: {name}")
    expected = [(profile["name"], mode) for profile in plan["profiles"] for mode in plan["order"]]
    require(len(data["positions"]) == len(expected) == 8, "Latency positions missing")
    all_codes = []
    for slot, (position, treatment) in enumerate(zip(data["positions"], expected), 1):
        require(position["slot"] == slot and (position["profile"], position["mode"]) == treatment,
                "Latency position order changed")
        require(position["status"] == "measured" and position["measurement_valid"] is True, "Invalid latency position")
        require(position["cpu_admission"] == {"native": "0", "guest": "19"}, "CPU placement changed")
        require(position["native_exit"] == {"code": 0, "signal": None}, "Native cleanup was not graceful")
        policy = position["socket_policy"]
        require(policy["requested_nodelay"] is True and policy["accepted_sockets"] == policy["verified_sockets"]
                == policy["verified_readback_true"] > 0 and policy["verified_readback_false"] == 0, "Socket policy changed")
        clock = position["clock"]
        require(clock["host_before_ms"] <= clock["host_after_ms"], "Clock bracket inverted")
        require(math.isclose(clock["offset_min_ms"], clock["host_before_ms"] - clock["browser_ms"], abs_tol=1e-7)
                and math.isclose(clock["offset_max_ms"], clock["host_after_ms"] - clock["browser_ms"], abs_tol=1e-7),
                "Clock arithmetic changed")
        window = position["browser_window"]
        require(math.isclose(window["end_ms"] - window["start_ms"], plan["window_ms"]), "Browser window changed")
        require(len(position["pulses"]) == 6, "Marker positions missing")
        timely = 0
        for index, pulse in enumerate(position["pulses"]):
            code = (slot - 1) * 6 + index + 1
            require(pulse["token"] == pulse["code"] == code, "Unique marker code changed")
            all_codes.append(code)
            require(pulse["planned_offset_ms"] == plan["pulse_offsets_ms"][index], "Source schedule changed")
            submitted = pulse["submitted_host_ms"]
            due = position["host_window_start_ms"] + pulse["planned_offset_ms"]
            require(math.isclose(pulse["submission_lateness_ms"], submitted - due, abs_tol=1e-7)
                    and 0 <= pulse["submission_lateness_ms"] <= plan["max_submission_lateness_ms"], "Source submission late")
            require(pulse["source_note"]["token"] == pulse["source_note"]["code"] == code
                    and pulse["source_valid"] is True, "Source confirmation missing")
            candidates = sorted((row for row in position["observations"] if row["code"] == code
                                 and row["analysis_begin_browser_ms"] < window["end_ms"]),
                                key=lambda row: row["host_received_ms"])
            if not candidates:
                require(pulse["status"] == "unobserved" and pulse["observation"] is None
                        and pulse["trigger_to_observed_ms"] is None, "Missing marker fabricated")
                continue
            observed = candidates[0]
            require(pulse["observation"] == observed and observed["analysis_begin_browser_ms"] >= window["start_ms"]
                    and observed["analysis_begin_browser_ms"] + clock["offset_min_ms"] >= submitted,
                    "Preexisting/ambiguous marker credited")
            elapsed = observed["host_received_ms"] - submitted
            require(elapsed >= 0 and math.isclose(elapsed, pulse["trigger_to_observed_ms"], abs_tol=1e-7),
                    "Marker latency arithmetic changed")
            status = "observed" if elapsed < plan["pulse_deadline_ms"] else "late"
            require(pulse["status"] == status, "Marker deadline outcome changed")
            timely += status == "observed"
        require(timely == position["timely_responses"] and position["complete_timely_set"] == (timely == 6),
                "Incomplete marker set marked complete")
        metrics = position["metrics"]
        require(metrics["observer_failure"] is None and metrics["video_errors_delta"] == 0, "Observer/video failed")
        require(all(0 <= row["at_ms"] < plan["window_ms"] for row in metrics["samples"]), "Sample outside fixed window")
        if position["profile"] == "kernel-limited":
            require(position["kernel_cleanup"]["root_removed"] is True, "Kernel queue cleanup unconfirmed")
            require(len(position["kernel_link"]["queues"]) == 2 and all(row["packets"] > 0 and row["drops"] == 0
                    and row["limit_packets"] == 1024 and row["delay_seconds"] == 0.04
                    and row["rate_bytes_per_second"] == 125000 for row in position["kernel_link"]["queues"]),
                    "Kernel treatment changed or dropped packets")
    require(all_codes == list(range(1, 49)), "Marker codes reused or omitted")
    for profile in plan["profiles"]:
        for mode in ["opus", "pcm"]:
            rows = [pulse for position in data["positions"] if position["profile"] == profile["name"]
                    and position["mode"] == mode for pulse in position["pulses"]]
            received = [row["trigger_to_observed_ms"] for row in rows if row["trigger_to_observed_ms"] is not None]
            print(f"{profile['name']} {mode}: {sum(row['status'] == 'observed' for row in rows)}/12 timely, "
                  f"{sum(row['status'] == 'late' for row in rows)} late, "
                  f"{sum(row['status'] == 'unobserved' for row in rows)} unobserved; "
                  f"received response range {min(received):.1f}–{max(received):.1f} ms")
    print("Verified all eight fixed windows and 48 unique marker outcomes, including late/missing responses.")


if __name__ == "__main__":
    main()
