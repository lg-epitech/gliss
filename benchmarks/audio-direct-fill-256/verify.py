"""Check original reports and recompute the direct-fill rejection without a browser."""
import gzip
import hashlib
import io
import json
import math
import statistics
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def read_archive(raw):
    result = {}
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(raw))) as archive:
        for member in archive:
            require(member.isfile() and not member.name.startswith('/')
                    and '..' not in Path(member.name).parts and member.name not in result,
                    'Invalid source member')
            result[member.name] = archive.extractfile(member).read()
    return result


def resources(value, tasks, deny_events=0):
    require(value['pids.max'] == tasks and 0 < value['pids.current'] <= value['pids.peak'] <= tasks,
            'Task boundary changed')
    require(value['pids.events']['max'] == deny_events, 'Task admission failed')
    require(value['memory.max'] == 2147483648 and 0 < value['memory.current'] < value['memory.max'],
            'Memory boundary changed')
    require(all(value['memory.events'][key] == 0 for key in ['max', 'oom', 'oom_kill']), 'Memory admission failed')


def verify_data(data, plan, registration):
    require(data['schema'] == 'gliss-audio-direct-fill-results-v1' and data['cleanup_complete'] is True
            and 'failure' not in data and data['page_errors'] == [], 'Incomplete component')
    require(data['source_pins'] == registration['sources'], 'Observed source pins changed')
    require(data['phase'] == 'cost-opus-baseline-enable', 'Last setup label changed')
    require(set(data['pools']) == {'pcm', 'opus'} and data['pools']['pcm']['packets'] == 64
            and data['pools']['pcm']['payload_bytes'] == 64 * 3840
            and data['pools']['opus']['packets'] == 65
            and all(len(p['payload_sha256']) == 64 and p['payload_bytes'] > 0 for p in data['pools'].values()),
            'Input/output pool accounting changed')
    require(plan['codecs'] == ['pcm', 'opus'] and plan['order'] == ['baseline', 'direct', 'direct', 'baseline']
            and plan['window_ms'] == 6000 and plan['warmup_ms'] == 1500 and plan['packet_frames'] == 960
            and plan['packets_per_window'] == 300 and plan['max_submission_lateness_ms'] == 100
            and plan['tasks_max'] == 256 and plan['positions_executed'] == 0, 'Fixed plan changed')
    require(plan['retention_gate'] == {'all_measurements_and_exact_sample_proofs_required': True,
            'each_codec_mean_browser_cpu_reduction_at_least_fraction': .1,
            'each_codec_candidate_cpu_range_below_baseline_range': True}, 'Retention gates changed')
    require([p['codec'] for p in data['sample_proofs']] == plan['codecs'], 'Sample proof missing')
    for proof in data['sample_proofs']:
        require(proof['exact_samples_equal'] is True and [r['mode'] for r in proof['modes']] == ['baseline', 'direct'],
                'Sample comparison incomplete')
        a, b = proof['modes']
        require(a['sha256'] == b['sha256'] and len(a['sha256']) == 64, 'Output samples differ')
        for arm in proof['modes']:
            s = arm['snapshot']
            require(arm['buffers'] == 8 and arm['bytes'] == 61440 and s['received'] == s['scheduled'] == 8
                    and s['pendingDecodes'] == 0 and s['enabled'] and s['codec'] == proof['codec'],
                    'Sample observation invalid')
    resources(data['resource_admission'], 256)
    expected = [(codec, mode) for codec in plan['codecs'] for mode in plan['order']]
    require(len(data['positions']) == 8, 'Fixed slots missing')
    for slot, (p, pair) in enumerate(zip(data['positions'], expected), 1):
        require(p['slot'] == slot and (p['codec'], p['mode']) == pair and p['status'] == 'measured'
                and p['measurement_valid'] is True, 'Position invalid')
        resources(p['resource_after'], 256)
        m = p['metrics'];before, after = m['before'], m['after']
        require(m['end_ms'] - m['start_ms'] == 6000 and m['isolated'] and m['errors'] == []
                and m['physical_speaker_latency_ms'] is None, 'Window scope changed')
        require(len(m['sends']) == len(m['schedule_timings']) == 300, 'Incomplete packet window')
        for i, row in enumerate(m['sends']):
            require(row['index'] == i and 0 <= row['lateness_ms'] <= 100 and 0 <= row['at_ms'] < 6000
                    and math.isclose(row['at_ms'] - i * 20, row['lateness_ms'], abs_tol=1e-6),
                    'Send outside its fixed window')
        require(all(0 <= r['at_ms'] < 6000 and math.isfinite(r['duration_ms']) and r['duration_ms'] >= 0
                    for r in m['schedule_timings']), 'Schedule callback outside window')
        require(after['received'] - before['received'] == after['scheduled'] - before['scheduled'] == 300
                and after['dropped'] == before['dropped'] and after['pendingDecodes'] == 0
                and 0 <= after['sources'] <= 12 and 0 <= after['queuedSeconds'] <= .2
                and after['enabled'] and after['codec'] == p['codec'], 'Playback or freshness gate changed')
        require(p['stopped']['sources'] == 0 and p['stopped']['contextState'] == 'closed'
                and p['stopped']['enabled'] is False, 'Player cleanup incomplete')
        cpu = p['cpu']
        require(cpu['complete_endpoint_set'] is True and cpu['matched_processes'] ==
                cpu['before_processes'] == cpu['after_processes'] and cpu['matched_processes'] > 0
                and 6000 <= cpu['elapsed_ms'] <= 7000 and cpu['cpu_seconds'] >= 0
                and math.isclose(cpu['cpu_seconds'] * 100000 / cpu['elapsed_ms'],
                                 cpu['percent_of_one_core'], abs_tol=1e-9), 'CPU counter window invalid')
    comparisons = []
    for codec in plan['codecs']:
        arms = {}
        for mode in ['baseline', 'direct']:
            rows = [p for p in data['positions'] if p['codec'] == codec and p['mode'] == mode]
            cpu = [p['cpu']['percent_of_one_core'] for p in rows]
            wall = [sum(t['duration_ms'] for t in p['metrics']['schedule_timings']) for p in rows]
            arms[mode] = {'cpu_percent': cpu, 'cpu_mean': statistics.mean(cpu), 'schedule_wall_ms': wall}
        fraction = 1 - arms['direct']['cpu_mean'] / arms['baseline']['cpu_mean']
        separated = max(arms['direct']['cpu_percent']) < min(arms['baseline']['cpu_percent'])
        comparisons.append({'codec': codec, 'arms': arms, 'mean_cpu_reduction_fraction': fraction,
                            'mean_reduction_gate_passed': fraction >= .1, 'range_gate_passed': separated})
    return comparisons


def main():
    record = json.loads((HERE / 'results.json').read_bytes())
    for name, expected in record['files'].items():
        require(digest((HERE / name).read_bytes()) == expected, name + ' changed')
    review = record['source_review']
    require(digest((HERE / review['file']).read_bytes()) == review['sha256'], 'Review disposition changed')
    plan = json.loads((HERE / 'plan.json').read_bytes());reg = json.loads((HERE / 'registration.json').read_bytes())
    require(reg['positions_executed'] == 0 and reg['status'] == 'prospective'
            and reg['plan_sha256'] == digest((HERE / 'plan.json').read_bytes()), 'Registration changed')
    sources = read_archive((HERE / 'source.tar.gz').read_bytes())
    for name, expected in {**reg['sources'], **reg['parent_sources']}.items():
        require(digest(sources[name]) == expected, 'Registered source changed')
    raw = gzip.decompress((HERE / 'cohort.json.gz').read_bytes())
    require(len(raw) == record['original_report_bytes'] and digest(raw) == record['original_report_sha256'], 'Original bytes changed')
    data = json.loads(raw)
    require(data['plan_sha256'] == reg['plan_sha256'], 'Report plan changed')
    comparisons = verify_data(data, plan, reg)
    decision = json.loads((HERE / 'decision.json').read_bytes())
    retained = all(c['mean_reduction_gate_passed'] and c['range_gate_passed'] for c in comparisons)
    require(comparisons == decision['comparisons'] and decision['all_measurements_and_exact_samples_valid']
            and decision['retain_candidate'] == retained and retained is False
            and decision['production_audio_player_changed'] is False,
            'Rejection decision changed')
    probe = record['setup_probe_inventory'];probe_root = (HERE / probe['file']).parent
    require(digest((HERE / probe['file']).read_bytes()) == probe['sha256'], 'Probe inventory changed')
    nav = json.loads((HERE / probe['file']).read_bytes())
    for name, expected in nav['files'].items():
        require(digest((probe_root / name).read_bytes()) == expected, 'Probe evidence changed')
    probe_reg = json.loads((probe_root / 'registration.json').read_bytes())
    probe_sources = read_archive((probe_root / 'source.tar.gz').read_bytes())
    require(probe_reg['positions_executed'] == 0 and probe_reg['order'] == [128, 256], 'Probe no longer prospective')
    for name, expected in probe_reg['sources'].items():require(digest(probe_sources[name]) == expected, 'Probe source changed')
    for ceiling in [128, 256]:
        p = json.loads((probe_root / f'probe-{ceiling}.json').read_bytes())
        require(p['cost_positions_executed'] == 0 and p['cleanup_complete'] and p['sources'] == probe_reg['sources'], 'Probe scope changed')
        terminal = p['checkpoints'][-1]['audit']
        resources(terminal, ceiling, deny_events=3 if ceiling == 128 else 0)
        require((p.get('setup_passed') is True) == (ceiling == 256), 'Setup outcome changed')
    for folder in ['rejected-setup', 'rejected-setup-diagnostic']:
        directory = HERE.parent / 'audio-direct-fill' / folder
        original = json.loads((directory / 'results.json').read_bytes())
        for name, expected in original['files'].items():require(digest((directory / name).read_bytes()) == expected, 'Rejected setup changed')
        raw = gzip.decompress((directory / 'cohort.json.gz').read_bytes())
        require(len(raw) == original['original_report_bytes'] and digest(raw) == original['original_report_sha256'], 'Rejected original bytes changed')
        failed = json.loads(raw)
        failed_reg = json.loads((directory / 'registration.json').read_bytes())
        failed_sources = read_archive((directory / 'source.tar.gz').read_bytes())
        require(failed_reg['positions_executed'] == 0 and failed['source_pins'] == failed_reg['sources']
                and failed['plan_sha256'] == digest((directory / 'plan.json').read_bytes()),
                'Rejected source registration changed')
        for name, expected in failed_reg['sources'].items():
            require(digest(failed_sources[name]) == expected, 'Rejected source archive changed')
        require(failed['failure'] == 'TimeoutError' and failed['cleanup_complete'] and len(failed['positions']) == 8
                and all(p['status'] == 'blocked' for p in failed['positions']), 'Failure relabeled as measured')
    for comparison in comparisons:
        print(comparison['codec'] + ': mean browser CPU reduction ' +
              f"{comparison['mean_cpu_reduction_fraction'] * 100:.2f}%; retention gates failed")
    print('Verified eight valid windows, 2400 timed packets, exact-sample observations, resource probes and both original setup failures. Candidate remains unserved.')


if __name__ == '__main__':
    main()
