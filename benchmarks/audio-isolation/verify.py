"""Verify the transport prototype's original reports without starting a guest."""
import hashlib
import importlib.util
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def main():
    inventory = json.loads((HERE / 'results.json').read_text())
    tool = ROOT / inventory['shared_verifier']
    require(digest(tool.read_bytes()) == inventory['shared_verifier_sha256'], 'Shared verifier changed')
    spec = importlib.util.spec_from_file_location('motion_evidence', tool)
    reference = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(reference)
    reference.HERE = HERE
    require(digest((HERE / inventory['validation']).read_bytes()) == inventory['validation_sha256'], 'Validation record changed')
    require([r['name'] for r in inventory['cohorts']] == ['eight', 'one'], 'Cohorts missing or reordered')
    total = 0
    for record in inventory['cohorts']:
        data = reference.verify(record)
        plan = json.loads((HERE / record['plan']).read_text())
        window = 8 if record['name'] == 'eight' else 1
        require(plan['video_in_flight_frames'] == window and plan['order'] == ['combined','split','split','combined'], 'Fixed treatment changed')
        for position in data['positions']:
            require(position['in_flight_frames'] == window and position['frame_skip'] is False, 'Native policy changed')
            require(position['relay_cleanup'] is True, 'Relay cleanup unconfirmed')
            argv = position['native_argv']
            for name, value in [('--in-flight-frames', str(window)), ('--h264-bitrate','500000'), ('--video-frame-skip','false')]:
                require(argv.count(name) == 1 and argv[argv.index(name) + 1] == value, 'Recorded native argv changed')
            require(argv.count('--stream-stats') == 1, 'Stats treatment changed')
            before, after = position['relay_before'], position['relay_after']
            require(before['mode'] == after['mode'] == position['mode'], 'Relay mode mismatch')
            require(after['failure'] is None and after['native_sessions'] == 1 and after['browser_sockets'] == 2,
                    'Single native/two browser socket proof missing')
            ids = after['identities']
            require(len(ids) == 2 and {row['role'] for row in ids} == {'main','companion'}, 'Socket roles changed')
            require(ids[0]['remote_port'] != ids[1]['remote_port'] and ids[0]['local_port'] == ids[1]['local_port'],
                    'Independent TCP sockets on one shared gateway port unproved')
            require(all(row['loopback'] and row['nodelay_requested'] for row in ids), 'Relay socket request changed')
            native_port = int(argv[argv.index('--bind') + 1].rsplit(':',1)[1])
            require(native_port != ids[0]['local_port'], 'Native hop and shaped gateway confused')
            if position['profile'] == 'kernel-limited':
                require(position['kernel_link']['target_port'] == ids[0]['local_port'], 'Both flows must share the aggregate shaper')
            c = after['counters']
            require(c['upstream_audio'] > 0 and c['upstream_video'] == c['main_video'] > 0, 'Media routing count changed')
            require(c['main_audio'] == (c['upstream_audio'] if position['mode'] == 'combined' else 0), 'Audio main routing changed')
            require(c['companion_audio'] == (c['upstream_audio'] if position['mode'] == 'split' else 0), 'Audio companion routing changed')
            require(c['overflows'] == c['protocol_errors'] == c['companion_pongs'] == 0, 'Relay failure hidden')
            require(all(type(v) is int and v >= before['counters'][k] for k,v in c.items()), 'Relay counters regressed')
            metrics = position['metrics']
            gaps = sum(b['sequence'] != ((a['sequence'] + 1) & 0xffffffff)
                       for a,b in zip(metrics['arrivals'],metrics['arrivals'][1:]))
            require(metrics['sequence_gaps'] == gaps == 0 and metrics['invalid_skip_stats'] == 0
                    and metrics['skip_samples'] == [], 'Video continuity/skipping invalid')
            require(position['cpu']['controller']['allowed_cpus'] == '0', 'Relay/controller CPU scope changed')
            total += len(position['pulses'])
    require(total == 96, 'Probe slots missing')
    print('Verified both eight-position prototypes, all 96 probe slots, distinct sockets on the shared link budget, and owned cleanup.')


if __name__ == '__main__':
    main()
