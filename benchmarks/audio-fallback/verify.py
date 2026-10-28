"""Check retained fallback positions and exact marker/cutoff arithmetic."""
import gzip
import hashlib
import io
import json
import math
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def require(value, reason):
    if not value:
        raise ValueError(reason)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def close(actual, expected):
    require(math.isclose(actual, expected, abs_tol=1e-7), "Arithmetic mismatch")


def main():
    manifest = json.loads((HERE / "results.json").read_text())
    registration_raw = (HERE / manifest["registration"]).read_bytes()
    require(digest(registration_raw) == manifest["registration_sha256"], "Prospective registration changed")
    registration = json.loads(registration_raw)
    require(registration["executed_positions"] == 0 and registration["binary_sha256"] == manifest["binary_sha256"], "Registration/binary mismatch")
    for name, expected in registration["sources"].items():
        require(digest((ROOT / name).read_bytes()) == expected, "Registered source changed")
    record = manifest["latency"]
    packed = (HERE / record["file"]).read_bytes()
    require(digest(packed) == record["compressed_sha256"], "Compressed evidence changed")
    raw = gzip.decompress(packed)
    require(digest(raw) == record["original_sha256"] and len(raw) <= 1048576, "Original evidence changed")
    data = json.loads(raw)
    plan_raw = (HERE / record["plan"]).read_bytes()
    require(digest(plan_raw) == record["plan_sha256"] == data["plan_sha256"], "Plan changed")
    plan = json.loads(plan_raw)
    require(data["schema"] == "gliss-audio-fallback-latency-results-v1", "Unknown evidence")
    require(data["binary_sha256"] == manifest["binary_sha256"], "Binary binding changed")
    require(data["cleanup_complete"] is True, "Guest cleanup unconfirmed")
    for name, expected in {**data["source_pins"], **manifest["runtime_source_pins"]}.items():
        require(digest((ROOT / name).read_bytes()) == expected, "Source changed: " + name)
    expected = [(profile["name"], mode) for profile in plan["profiles"] for mode in plan["order"]]
    require(len(expected) == len(data["positions"]) == 12, "Positions missing")
    codes = []
    for slot, (position, treatment) in enumerate(zip(data["positions"], expected), 1):
        require(position["slot"] == slot and (position["profile"], position["mode"]) == treatment, "Position moved")
        require(position["status"] == "measured" and position["measurement_valid"] is True, "Failed/blocked measurement")
        require(position["native_exit"] == {"code": 0, "signal": None}, "Native cleanup failed")
        require(position["cpu_admission"] == {"native": "0", "guest": "19"}, "CPU admission changed")
        policy = position["socket_policy"]
        require(policy["requested_nodelay"] is True and policy["accepted_sockets"] == policy["verified_sockets"]
                == policy["verified_readback_true"] > 0 and policy["verified_readback_false"] == 0, "TCP readback failed")
        clock = position["clock"]
        require(clock["host_before_ms"] <= clock["host_after_ms"], "Clock bracket inverted")
        close(clock["offset_min_ms"], clock["host_before_ms"] - clock["browser_ms"])
        close(clock["offset_max_ms"], clock["host_after_ms"] - clock["browser_ms"])
        window = position["browser_window"]
        close(window["end_ms"] - window["start_ms"], plan["window_ms"])
        require(len(position["pulses"]) == 4, "Markers missing")
        timely = 0
        for index, pulse in enumerate(position["pulses"]):
            code = (slot - 1) * 4 + index + 1
            codes.append(code)
            require(pulse["token"] == pulse["code"] == code, "Unique source code changed")
            require(pulse["planned_offset_ms"] == plan["pulse_offsets_ms"][index], "Schedule changed")
            submitted = pulse["submitted_host_ms"]
            close(pulse["submission_lateness_ms"], submitted - position["host_window_start_ms"] - pulse["planned_offset_ms"])
            require(0 <= pulse["submission_lateness_ms"] <= plan["max_submission_lateness_ms"], "Late submission")
            require(pulse["source_note"]["token"] == pulse["source_note"]["code"] == code
                    and pulse["source_valid"] is True, "Source unconfirmed")
            candidates = sorted((row for row in position["observations"] if row["code"] == code
                                 and row["analysis_begin_browser_ms"] < window["end_ms"]), key=lambda row: row["host_received_ms"])
            if not candidates:
                require(pulse["status"] == "unobserved" and pulse["observation"] is None
                        and pulse["trigger_to_observed_ms"] is None, "Missing marker fabricated")
                continue
            observation = candidates[0]
            require(pulse["observation"] == observation, "Observation changed")
            require(observation["analysis_begin_browser_ms"] >= window["start_ms"]
                    and observation["analysis_begin_browser_ms"] + clock["offset_min_ms"] >= submitted, "Preexisting marker credited")
            elapsed = observation["host_received_ms"] - submitted
            require(elapsed >= 0, "Negative latency")
            close(pulse["trigger_to_observed_ms"], elapsed)
            require(pulse["status"] == ("observed" if elapsed < plan["pulse_deadline_ms"] else "late"), "Cutoff changed")
            timely += pulse["status"] == "observed"
        require(position["timely_responses"] == timely and position["complete_timely_set"] == (timely == 4), "Count changed")
        metrics = position["metrics"]
        require(metrics["observer_failure"] is None and metrics["video_errors_delta"] == 0
                and metrics["quality_windows_dropped"] == 0, "Observer/video failure")
        require(all(row["queuedSeconds"] <= 0.2 + 1e-7 and row["sources"] <= 12 for row in metrics["samples"]), "Playback bounds exceeded")
        wire = metrics["wire_bytes"] * 8 * 48000 / metrics["wire_frames"] / 1000
        if position["mode"] == "ima-adpcm":
            close(wire, 394.8)
        elif position["mode"] == "pcm":
            close(wire, 1544)
        if position["profile"] == "kernel-limited":
            require(all(q["packets"] > 0 and q["drops"] == 0 for q in position["kernel_link"]["queues"]), "Kernel loss")
            require(position["kernel_cleanup"]["root_removed"] is True, "Queue cleanup unconfirmed")
        responses = [p["trigger_to_observed_ms"] for p in position["pulses"] if p["status"] == "observed"]
        span = f"{min(responses):.1f}–{max(responses):.1f} ms" if responses else "no timely responses"
        print(f"slot {slot}, {position['profile']}, {position['mode']}: {timely}/4 timely; {span}; {wire:.1f} kbit/s of received samples")
    require(codes == list(range(1, 49)), "Source IDs missing/reused")
    for record in manifest["components"]:
        packed = (HERE / record["file"]).read_bytes()
        require(digest(packed) == record["compressed_sha256"], "Component report changed")
        report_raw = gzip.decompress(packed)
        require(digest(report_raw) == record["original_sha256"], "Original component report changed")
        report = json.loads(report_raw)
        require(report["error"] is None and report["webcodecs_disabled"] is True, "Component failed")
        require(report["input_sha256"] == record["native_input_sha256"], "Component input binding changed")
        require(len(report["arms"]) == record["arms"], "Component arms missing")
        for arm in report["arms"]:
            if "decoded_frames" not in arm:
                continue
            require(arm["error"] is None and len(arm["decoded_frames"]) == arm["files"], "Component decode failed")
            require(all(n == arm["frames"] for n in arm["decoded_frames"]), "Component duration changed")
            require(len(arm["decode_wall_ms"]) == arm["files"], "Decode timings missing")
            require(all(math.isfinite(v) for v in arm["quality"].values()), "Invalid distortion metric")
            if arm["mode"] == "ima-adpcm":
                require(arm["max_native_difference"] == 0 and arm["payload_bytes"] == 48350, "ADPCM cross-language mismatch")
        for key in ["plan", "source_archive"]:
            require(digest((HERE / record[key]).read_bytes()) == record[key + "_sha256"], "Component source/declaration changed")
        files = archive_files(record["source_archive"])
        if "inherited_archive" in record:
            files = {**archive_files(record["inherited_archive"]), **files}
        declaration = json.loads((HERE / record["plan"]).read_text())
        for name, expected in {**declaration["sources"], **declaration.get("extra_sources", {})}.items():
            content = files[name] if name in files else (ROOT / name).read_bytes()
            require(digest(content) == expected, "Archived component source binding changed")
    component = manifest["guest_component"]
    packed = (HERE / component["file"]).read_bytes()
    require(digest(packed) == component["compressed_sha256"], "Guest component changed")
    log = gzip.decompress(packed)
    require(digest(log) == component["original_sha256"], "Original guest component changed")
    rows = [json.loads(line[2:]) for line in log.decode().splitlines() if line.startswith("ℹ {")]
    require([row["codec"] for row in rows] == ["opus", "ima-adpcm", "pcm"], "Guest codec positions changed")
    for row in rows:
        require(row["mix_frequency"] == 48000 and row["state"]["video_errors"] == row["state"]["audio"]["dropped"] == 0
                and row["state"]["probe_failure"] is None, "Guest component failed")
        require([pulse["code"] for pulse in row["pulses"]] == [1,10,20,30,40,48], "Guest source positions changed")
        for pulse in row["pulses"]:
            require(pulse["source_note"]["token"] == pulse["token"] and pulse["source_note"]["code"] == pulse["code"] == pulse["observation"]["code"], "Guest source attribution changed")
            close(pulse["trigger_to_observed_ms"], pulse["observation"]["host_received_ms"] - pulse["submitted_host_ms"])
            require(0 <= pulse["trigger_to_observed_ms"] < 1500, "Guest marker missed deadline")
    print("Verified twelve positions, 48 unique probes, exact packet rates, and all four component reports. Quality values are retained observations; replay the probe to recompute them.")
    verify_cost(manifest)


def archive_files(name):
    packed = (HERE / name).read_bytes()
    raw = gzip.decompress(packed)
    require(len(raw) <= 262144, "Source archive exceeds cap")
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
        members = archive.getmembers()
        require(len(members) <= 8 and all(m.isfile() for m in members), "Unexpected archive members")
        return {member.name: archive.extractfile(member).read() for member in members}


def verify_cost(manifest):
    record = manifest["cost"]
    packed = (HERE / record["file"]).read_bytes()
    require(digest(packed) == record["compressed_sha256"], "Cost evidence changed")
    raw = gzip.decompress(packed)
    require(digest(raw) == record["original_sha256"] and len(raw) <= 1048576, "Original cost evidence changed")
    data = json.loads(raw)
    plan_raw = (HERE / record["plan"]).read_bytes()
    require(digest(plan_raw) == data["plan_sha256"] == record["plan_sha256"], "Cost plan changed")
    plan = json.loads(plan_raw)
    require(data["schema"] == "gliss-audio-fallback-cost-results-v1" and data["binary_sha256"] == manifest["binary_sha256"], "Cost binary/schema changed")
    require(data["cleanup_complete"] is True and data["guest_cpu"] == 19 and data["native_browser_cpu"] == 0, "Cost cleanup/CPU placement failed")
    require(len(data["positions"]) == len(plan["order"]) == 8, "Cost positions missing")
    for source, expected in data["source_pins"].items():
        require(digest((ROOT / source).read_bytes()) == expected, "Cost source changed")
    for slot, (position, mode) in enumerate(zip(data["positions"], plan["order"]), 1):
        require(position["slot"] == slot and position["mode"] == mode and position["status"] == "measured"
                and position["profile"] == "loopback", "Cost position moved/failed")
        metrics = position["metrics"]
        require(metrics["window_ms"] == 6000 and metrics["video_errors_delta"] == 0, "Cost window/video failure")
        for kind, cpu in [("native", "0"), ("guest", "19")]:
            sample = metrics[kind + "_cpu"]
            require(sample["allowed_cpus"] == cpu, "Cost CPU placement changed")
            close(sample["percent_of_one_core"], sample["ticks"] / sample["ticks_per_second"] * 100000 / sample["elapsed_ms"])
        browser = metrics["browser_cpu"]
        require(browser["complete_endpoint_set"] is True and browser["matched_processes"] == browser["before_processes"] == browser["after_processes"], "Browser counters incomplete")
        close(browser["percent_of_one_core"], browser["cpu_seconds"] * 100000 / browser["elapsed_ms"])
        require(len(metrics["pings"]) == metrics["ping"]["sent"] == 20, "Cost control probes missing")
        replies = []
        for ping in metrics["pings"]:
            require(0 <= ping["sent_ms"] < metrics["window_ms"], "Probe outside window")
            if ping["received_ms"] is not None:
                require(ping["sent_ms"] <= ping["received_ms"] < metrics["window_ms"], "Reply outside window")
                replies.append(ping["received_ms"] - ping["sent_ms"])
        summary = metrics["ping"]
        require(summary["completed"] == len(replies) and summary["complete"] == (len(replies) == 20)
                and summary["missed_at_cutoff"] == 20 - len(replies), "Control summary changed")
        if replies:
            ordered = sorted(replies)
            close(summary["responded_p95_ms"], ordered[math.ceil(len(ordered) * 0.95) - 1])
        wire = metrics["audio_wire"]
        close(metrics["audio_received_kbit_s"], wire["bytes"] * 8 / metrics["window_ms"])
        if mode == "off":
            require(wire["frames"] == wire["bytes"] == 0, "Muted position has sound")
        else:
            rate = wire["bytes"] * 8 * 48000 / wire["frames"] / 1000
            close(metrics["audio_kbit_per_second_of_received_samples"], rate)
            if mode in ["pcm", "ima-adpcm"]:
                close(rate, 1544 if mode == "pcm" else 394.8)
        print(f"cost slot {slot}, {mode}: native {metrics['native_cpu']['percent_of_one_core']:.2f}%, browser {browser['percent_of_one_core']:.2f}% of one core; {len(replies)}/20 control replies")


if __name__ == "__main__":
    main()
