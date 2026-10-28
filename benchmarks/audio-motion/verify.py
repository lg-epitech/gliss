"""Recompute the retained motion/audio comparisons without launching a guest."""
import gzip
import hashlib
import io
import json
import math
import sys
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(ROOT / 'benchmarks'))
from runtime_snapshot import runtime_source_bytes


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def close(a, b):
    return math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-6)


def bound_file(record, key):
    raw = (HERE / record[key]).read_bytes()
    require(digest(raw) == record[key + '_sha256'], key + ' hash changed')
    return raw


def percentile(rows, fraction):
    return sorted(rows)[max(0, math.ceil(len(rows) * fraction) - 1)] if rows else None


def verify(record):
    plan_raw = bound_file(record, 'plan')
    plan = json.loads(plan_raw)
    registration = json.loads(bound_file(record, 'registration'))
    require(registration['positions_executed'] == 0, 'Registration no longer prospective')
    archive = gzip.decompress(bound_file(record, 'source_archive'))
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        sources = {}
        for item in tar:
            require(item.isfile() and not item.name.startswith('/') and '..' not in Path(item.name).parts,
                    'Invalid source archive member')
            require(item.name not in sources, 'Duplicate archived source')
            sources[item.name] = tar.extractfile(item).read()
    for name, expected in registration['sources'].items():
        source = sources.get(name)
        if source is None:
            source = runtime_source_bytes(name)
        require(digest(source) == expected, 'Registered source changed: ' + name)
    raw = gzip.decompress(bound_file(record, 'results'))
    require(len(raw) == record['original_bytes'] and digest(raw) == record['original_sha256'], 'Original result bytes changed')
    data = json.loads(raw)
    require(data['plan_sha256'] == digest(plan_raw), 'Result plan binding changed')
    require(data['binary_sha256'] == registration['binary_sha256'] == record['binary_sha256'], 'Executable binding changed')
    require(data['cleanup_complete'] and data['physical_speaker_latency_ms'] is None, 'Cleanup/measurement scope changed')
    for name, expected in data['source_pins'].items():
        registered = registration['sources'].get(name)
        if registered is None:
            require(record['observed_unregistered_source_pins'].get(name) == expected,
                    'Unqualified source observation: ' + name)
            require(digest(runtime_source_bytes(name)) == expected, 'Observed parent source changed')
        else:
            require(registered == expected, 'Observed source differs from registration: ' + name)
    if record['name'] != 'baseline':
        for name in ['kernel_sha256', 'initramfs_sha256', 'init_binary_sha256']:
            value = data['guest_build'][name]
            require(isinstance(value, str) and len(value) == 64 and all(x in '0123456789abcdef' for x in value), 'Guest hash missing')
    expected = [(p['name'], mode) for p in plan['profiles'] for mode in plan['order']]
    require(len(data['positions']) == len(expected) == 8, 'Fixed positions missing')
    codes = []
    for slot, (position, pair) in enumerate(zip(data['positions'], expected), 1):
        require((position['profile'], position['mode']) == pair and position['slot'] == slot, 'Position order changed')
        # Retain failures; they do not contribute a performance comparison.
        if position['status'] != 'measured':
            print(f"{record['name']} slot {slot}: {position['status']} (no performance claim)")
            continue
        require(position['native_exit'] == {'code': 0, 'signal': None}, 'Native exit unconfirmed')
        require(position['cpu_admission'] == {'native': '0', 'guest': '19'}, 'CPU scope changed')
        m = position['metrics']
        require(close(m['end_ms'] - m['start_ms'], plan['window_ms']), 'Browser window changed')
        require(position['browser_window'] == {'start_ms': m['start_ms'], 'end_ms': m['end_ms']}, 'Window attribution changed')
        require(m['overflow'] is False and m['observer_failure'] is None and m['video_errors_delta'] == 0, 'Observer invalid')
        require(m['invalid_stamp_paints'] == sum(p['source_frame'] is None for p in m['paints']), 'Paint count changed')
        require(m['video_marker_eligible'] == (m['invalid_stamp_paints'] == 0), 'Paint eligibility changed')
        require(m['unique_source_frames'] == len({p['source_frame'] for p in m['paints'] if p['source_frame'] is not None}), 'Unique marker count changed')
        require(all(0 <= p['at_ms'] < plan['window_ms'] for p in m['paints']), 'Paint outside window')
        require(m['video_wire_bytes'] == sum(a['bytes'] for a in m['arrivals']), 'Video byte count changed')
        require(all(0 <= a['at_ms'] < plan['window_ms'] for a in m['arrivals']), 'Arrival outside window')
        require(len(m['paints']) <= plan['paint_cap_per_position'] and len(m['arrivals']) <= plan['paint_cap_per_position'], 'Observer cap exceeded')
        require(len(m['pings']) == plan['control_probes_per_position'] == 20, 'Probe slots missing')
        for i, ping in enumerate(m['pings']):
            require(ping['id'] == i and ping['planned_ms'] == i * plan['ping_interval_ms'], 'Probe identity changed')
            for field in ['sent_ms', 'wire_received_ms', 'dispatched_ms']:
                require(ping[field] is None or 0 <= ping[field] < plan['window_ms'], 'Control timestamp outside window')
            if record['name'] != 'baseline' and ping['wire_received_ms'] is not None and ping['dispatched_ms'] is not None:
                require(ping['dispatched_ms'] >= ping['wire_received_ms'], 'Ordered callback witness violated')
        for name, cpu in position['cpu'].items():
            require(cpu['elapsed_ms'] > 0, 'CPU window missing')
            seconds = cpu['cpu_seconds'] if name == 'browser' else cpu['ticks'] / cpu['ticks_per_second']
            require(close(seconds * 100000 / cpu['elapsed_ms'], cpu['percent_of_one_core']), 'CPU arithmetic changed')
            if name == 'browser':
                require(cpu['complete_endpoint_set'] and cpu['matched_processes'] == cpu['before_processes'] == cpu['after_processes'], 'Incomplete browser CPU endpoint set')
        require(len(position['pulses']) == 6, 'Audio probe slots missing')
        timely = 0
        for i, pulse in enumerate(position['pulses']):
            code = (slot - 1) * 6 + i + 1
            codes.append(code)
            require(pulse['code'] == pulse['token'] == code and pulse['planned_offset_ms'] == plan['pulse_offsets_ms'][i], 'Audio probe identity changed')
            note = pulse['source_note']
            require(pulse['source_valid'] and note['token'] == note['code'] == code, 'Guest source unconfirmed')
            lateness = pulse['submitted_host_ms'] - position['host_window_start_ms'] - pulse['planned_offset_ms']
            require(close(lateness, pulse['submission_lateness_ms']) and lateness <= plan['max_submission_lateness_ms'], 'Submission gate changed')
            observation = pulse['observation']
            if observation is None:
                require(pulse['status'] == 'unobserved' and pulse['trigger_to_observed_ms'] is None, 'Missing response relabeled')
                continue
            require(observation['code'] == code, 'Output code mismatch')
            elapsed = observation['host_received_ms'] - pulse['submitted_host_ms']
            require(close(elapsed, pulse['trigger_to_observed_ms']), 'Response arithmetic changed')
            require(m['start_ms'] <= observation['analysis_begin_browser_ms'] < m['end_ms'], 'Output outside window')
            require(observation['analysis_begin_browser_ms'] + position['clock']['offset_min_ms'] >= pulse['submitted_host_ms'], 'Preexisting response accepted')
            require(pulse['status'] == ('observed' if elapsed < plan['pulse_deadline_ms'] else 'late'), 'Deadline status changed')
            timely += pulse['status'] == 'observed'
        require(position['timely_responses'] == timely and position['complete_timely_set'] == (timely == 6), 'Audio success count changed')
        if position['profile'] == 'kernel-limited':
            require(position['kernel_cleanup']['root_removed'] is True, 'Private shaper cleanup unconfirmed')
            for q in position['kernel_link']['queues']:
                require(q['packets'] > 0 and q['drops'] == 0 and q['rate_bytes_per_second'] == 125000 and q['delay_seconds'] == .04, 'Kernel treatment changed')
        if record['name'] == 'skip':
            require(position['frame_skip'] == plan['frame_skip'][position['mode']], 'Skip treatment changed')
            gaps = sum(b['sequence'] != ((a['sequence'] + 1) & 0xffffffff) for a, b in zip(m['arrivals'], m['arrivals'][1:]))
            require(m['sequence_gaps'] == gaps == 0 and m['invalid_skip_stats'] == 0, 'Media continuity invalid')
            samples = m['skip_samples']
            if position['frame_skip']:
                require(0 < len(samples) <= 16, 'Native skip samples missing')
                require(all(type(s['count']) is int and s['count'] >= 0 and 0 <= s['at_ms'] < plan['window_ms'] for s in samples), 'Malformed skip sample')
                require(all(b['count'] >= a['count'] for a,b in zip(samples, samples[1:])), 'Skip counter regressed')
            else:
                require(samples == [], 'Absent native counter inferred as zero')
        require(position['measurement_valid'], 'Failed measurement cannot support summary')
        wire = (m['wire_bytes'] + m['video_wire_bytes']) * 8 / plan['window_ms']
        latencies = [p['trigger_to_observed_ms'] for p in position['pulses'] if p['status'] == 'observed']
        replies = [p['wire_received_ms'] - p['sent_ms'] for p in m['pings'] if p['wire_received_ms'] is not None]
        reply95 = percentile(replies, .95)
        label = 'complete' if len(replies) == 20 else 'conditional'
        print(f"{record['name']} {slot} {position['profile']} {position['mode']}: "
              f"{timely}/6 timely audio; {wire:.1f} kbit/s app media; "
              f"{m['unique_source_frames']/6:.2f} distinct painted marker IDs/s; "
              f"{len(replies)}/20 pongs, {label} wire p95 {reply95:.1f} ms" if reply95 is not None else f"{record['name']} {slot}: no control reply")
    require(codes == list(range(1,49)), 'Measured marker positions missing')
    return data


def main():
    inventory = json.loads((HERE / 'results.json').read_text())
    require(digest((HERE / inventory['validation']).read_bytes()) == inventory['validation_sha256'], 'Validation record changed')
    for record in inventory['cohorts']:
        verify(record)
    print('Verified 24 fixed positions, 144 audio probe slots, original hashes, source declarations, CPU arithmetic and scoped delivery metrics.')


if __name__ == '__main__':
    main()
