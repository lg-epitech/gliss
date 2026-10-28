"""Check retained QP-floor pairs without launching an encoder or guest."""
import gzip
import hashlib
import importlib.util
import io
import json
import math
import statistics
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
VARIABLE_FIELDS = {'schema', 'order', 'frame_skip', 'qp_bounds', 'qualification'}


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def bound_json(directory, filename, expected):
    raw = (directory / filename).read_bytes()
    require(digest(raw) == expected, filename + ' changed')
    return json.loads(raw)


def read_record(directory, record):
    packed = (directory / record['file']).read_bytes()
    require(digest(packed) == record['compressed_sha256'], 'Compressed observations changed')
    raw = gzip.decompress(packed)
    require(len(raw) == record['original_bytes'] and digest(raw) == record['original_sha256'],
            'Original observations changed')
    return raw


def check_plan(plan):
    parent = json.loads((ROOT / 'benchmarks/video-qp/plan.json').read_bytes())
    fixed = lambda value: {k: v for k, v in value.items() if k not in VARIABLE_FIELDS}
    require(fixed(plan) == fixed(parent), 'Fixed workload, gates or resource limits changed')
    require(plan['schema'] in ['gliss-video-qp-floor-plan-v1', 'gliss-video-qp-range-plan-v1'],
            'Unknown plan')
    low, high, *rest = plan['order']
    require(rest == [high, low] and low != high, 'Pair order changed')
    require(set(plan['qp_bounds']) == set(plan['frame_skip']) == {low, high}, 'Pair policies changed')
    for name in [low, high]:
        q = plan['qp_bounds'][name]
        require(set(q) == {'min', 'max'} and type(q['min']) is int and 1 <= q['min'] <= 51
                and type(q['max']) is int and q['max'] == 51 and name == 'floor' + str(q['min'])
                and plan['frame_skip'][name] is False, 'Invalid QP policy')
    require(plan['qp_bounds'][low]['min'] < plan['qp_bounds'][high]['min'], 'Unordered QP pair')
    return low, high


def frame_values(arm):
    return [{k: v for k, v in frame.items() if k != 'encode_wall_ns'} for frame in arm['frames']]


def verify_study(directory):
    directory = Path(directory).resolve()
    inventory = json.loads((directory / 'results.json').read_bytes())
    require(inventory['schema'] == 'gliss-video-qp-pair-inventory-v1', 'Unknown inventory')
    validation = bound_json(directory, 'validation.json', inventory['validation_sha256'])
    require(validation['portable_tests'] == {'passed': 180, 'failed': 0, 'skipped': 0},
            'Portable validation incomplete')
    require(validation['production_runtime_unchanged'] and not validation['distribox_changes']
            and not validation['release_defaults_changed'], 'Runtime scope changed')
    tool = ROOT / inventory['shared_verifier']
    require(digest(tool.read_bytes()) == inventory['shared_verifier_sha256'], 'Shared verifier changed')
    spec = importlib.util.spec_from_file_location('qp_pair_motion_evidence', tool)
    shared = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(shared)
    shared.HERE = directory
    plan = bound_json(directory, inventory['cohort']['plan'], inventory['cohort']['plan_sha256'])
    low, high = check_plan(plan)
    data = shared.verify(inventory['cohort'])
    for p in data['positions']:
        require(p['status'] == 'measured' and p['measurement_valid'], 'Incomplete live position')
        q = plan['qp_bounds'][p['mode']]
        require(p['qp_bounds'] == {**q, 'scope': 'requested native bounds; not coded-QP attestation'}
                and p['frame_skip'] is False and p['in_flight_frames'] == 8, 'Live QP policy changed')
        argv = p['native_argv']
        for flag, value in [('--in-flight-frames', '8'), ('--h264-bitrate', '500000'),
                            ('--video-frame-skip', 'false'), ('--video-qp-min', str(q['min'])),
                            ('--video-qp-max', str(q['max']))]:
            require(argv.count(flag) == 1 and argv[argv.index(flag) + 1] == value, 'CLI mismatch')
        require(argv.count('--stream-stats') == 1, 'Missing shared stats')
        m = p['metrics']
        require(m['invalid_video_headers'] == m['sequence_gaps'] == m['invalid_skip_stats'] == 0
                and m['skip_samples'] == [], 'Invalid video diagnostics')
        require(all(type(a['keyframe']) is bool and a['bytes'] >= 25 for a in m['arrivals']),
                'Invalid picture flags')
        require(all(b['sequence'] == ((a['sequence'] + 1) & 0xffffffff)
                    for a, b in zip(m['arrivals'], m['arrivals'][1:])), 'Video gap')
    component_plan = bound_json(directory, 'component-plan.json', inventory['component_plan_sha256'])
    reg = bound_json(directory, 'component-registration.json', inventory['component_registration_sha256'])
    arguments = [plan['qp_bounds'][low]['min'], plan['qp_bounds'][high]['min'], 51]
    require(reg['positions_executed'] == component_plan['positions_executed'] == 0
            and reg['plan_sha256'] == inventory['component_plan_sha256']
            and reg['input_bytes'] == component_plan['input_bytes'] == 49152000
            and reg['input_sha256'] == '824c59818eb68261460a5c44f3746bb7077b94931339aa5db38f34079169466c'
            and reg['arguments'] == component_plan['range_arguments'] == arguments,
            'Component registration invalid')
    require(component_plan['order'] == plan['order'] and component_plan['qp_bounds'] == plan['qp_bounds']
            and component_plan['geometry'] == [1280, 800] and component_plan['frames_per_arm'] == 12
            and component_plan['source_frames'] == list(range(0, 34, 3))
            and component_plan['profile'] == 'screen' and component_plan['bitrate_target'] == 500000
            and component_plan['timing'] == {'fps': 30, 'timestamp_fps': 30, 'intra_period': 60}
            and component_plan['frame_skip'] is False, 'Component workload changed')
    packed = (directory / 'component-source.tar.gz').read_bytes()
    require(digest(packed) == inventory['component_source_archive_sha256'], 'Component archive changed')
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(packed))) as archive:
        sources = {}
        for member in archive:
            require(member.isfile() and not member.name.startswith('/')
                    and '..' not in Path(member.name).parts and member.name not in sources,
                    'Invalid archived member')
            sources[member.name] = digest(archive.extractfile(member).read())
        require(all(sources.get(name) == value == component_plan['source_pins'][name]
                    for name, value in reg['sources'].items()), 'Component source changed')
    component = json.loads(read_record(directory, inventory['component']))
    journal_raw = read_record(directory, inventory['component_journal'])
    journal = [json.loads(line) for line in journal_raw.splitlines()]
    require(component['schema'] == 'gliss-video-qp-range-component-v1'
            and component['frames_per_arm'] == 12 and len(component['arms']) == 4, 'Missing component arms')
    expected_journal = [{'event': 'start', 'order': [False, True, True, False], 'frames_per_arm': 12}]
    for index, arm in enumerate(component['arms']):
        candidate = index in [1, 2]
        require(arm['candidate'] is candidate and arm['qp_min'] == arguments[int(candidate)]
                and arm['qp_max'] == 51 and len(arm['frames']) == 12, 'Wrong component arm')
        expected_journal.append({'event': 'arm-start', 'arm': index, 'candidate': candidate})
        for i, row in enumerate(arm['frames']):
            require(row['index'] == i and row['source_counter'] == i * 3 and row['bytes'] > 24
                    and row['keyframe'] is (i == 0) and row['encode_wall_ns'] > 0, 'Invalid frame')
            expected_journal.extend([{'event': 'attempt', 'arm': index, 'index': i},
                                     {'event': 'frame', 'arm': index, 'data': row}])
            for prefix in ['rgb', 'document', 'texture']:
                mae, mse = row[prefix + '_mae'], row[prefix + '_mse']
                require(math.isfinite(mae) and math.isfinite(mse) and 0 <= mae <= 255
                        and mae * mae <= mse + 1e-6 and mse <= 65025, 'Invalid error metric')
            for suffix in ['mae', 'mse']:
                require(row['rgb_' + suffix] * 1280 * 800 + 1e-5 >=
                        row['document_' + suffix] * 950 * 460 + row['texture_' + suffix] * 360 * 180,
                        'Inconsistent region error')
        print(f"component {index + 1} {arm['qp_min']}..51: "
              f"{sum(f['bytes'] for f in arm['frames'])} bytes; "
              f"document MAE {statistics.mean(f['document_mae'] for f in arm['frames']):.3f}; "
              f"texture MAE {statistics.mean(f['texture_mae'] for f in arm['frames']):.3f}")
    expected_journal.append({'event': 'complete'})
    require(journal == expected_journal and len(journal) == 102
            and len(journal) <= component_plan['limits']['journal_records_max']
            and len(journal_raw) <= component_plan['limits']['journal_bytes_max'], 'Incomplete journal')
    for a, b in [(0, 3), (1, 2)]:
        require(frame_values(component['arms'][a]) == frame_values(component['arms'][b]),
                'Repeated component observations changed')
    reference = inventory['component_control_reference']
    packed = (directory / reference['file']).read_bytes()
    require(digest(packed) == reference['compressed_sha256'], 'Frozen control reference changed')
    parent = json.loads(gzip.decompress(packed))
    require(frame_values(component['arms'][0]) == frame_values(parent['arms'][reference['arm_index']]),
            'Shared-input control differs from its frozen parent')
    print('Verified eight live positions, all 48 sound probes, four image arms and all journal records. '
          'Replay to remeasure fidelity.')
    return data


if __name__ == '__main__':
    verify_study(Path(__file__).resolve().parent)
