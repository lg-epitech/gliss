"""Check native 20/40 ms packet accounting through the existing Chromium player."""
import gzip
import hashlib
import io
import json
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def main():
    inventory = json.loads((HERE / 'results.json').read_bytes())
    for name, value in inventory['files'].items():
        require(digest((HERE / name).read_bytes()) == value, name + ' changed')
    decoded = {}
    for name, record in inventory['records'].items():
        packed = (HERE / record['file']).read_bytes()
        require(digest(packed) == record['compressed_sha256'], 'Compressed observations changed')
        raw = gzip.decompress(packed)
        require(digest(raw) == record['original_sha256'] and len(raw) == record['original_bytes'],
                'Original observations changed')
        decoded[name] = raw
    plan = json.loads((HERE / 'plan.json').read_bytes())
    registration = json.loads((HERE / 'registration.json').read_bytes())
    require(registration['status'] == 'prospective' and registration['positions_executed'] == 0
            and registration['plan_sha256'] == digest((HERE / 'plan.json').read_bytes()), 'Registration mismatch')
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress((HERE / 'source.tar.gz').read_bytes()))) as archive:
        sources = {}
        for member in archive:
            require(member.isfile() and not member.name.startswith('/')
                    and '..' not in Path(member.name).parts and member.name not in sources, 'Invalid source archive')
            sources[member.name] = archive.extractfile(member).read()
    require(set(sources) == set(registration['sources']), 'Source closure changed')
    for name, value in registration['sources'].items():require(digest(sources[name]) == value, 'Source bytes changed')
    native = json.loads(decoded['native.json']);browser = json.loads(decoded['browser.json'])
    journal = [json.loads(line) for line in decoded['native.progress.jsonl'].splitlines()]
    require(plan['order_frames'] == [960, 1920, 1920, 960] and plan['source_frames'] == 15360
            and plan['sample_rate'] == 48000 and plan['channels'] == 2, 'Source or case order changed')
    require(native['schema'] == 'gliss-audio-duration-prerequisite-v1' and native['sample_rate'] == 48000
            and native['channels'] == 2 and native['source_frames'] == 15360
            and len(native['arms']) == len(browser['positions']) == 4, 'Component positions missing')
    require(browser['native_report_sha256'] == digest(decoded['native.json'])
            and browser['cleanup_complete'] and 'failure' not in browser, 'Browser binding or cleanup failed')
    for name, value in browser['sources'].items():require(registration['sources'][name] == value, 'Browser source drift')
    expected_journal = [{'event': 'start', 'frames': 15360, 'source_sha256': native['source_pcm_sha256']}]
    packets = 0
    for index, frames in enumerate(plan['order_frames']):
        arm, p = native['arms'][index], browser['positions'][index]
        count = 15360 // frames + 1
        require(arm['packet_frames'] == frames and arm['source_frames'] == 15360
                and arm['padding_packets'] == 1 and arm['native_decoded_frames'] == count * frames
                and arm['lookahead_frames'] == 312 and len(arm['chunks']) == count, 'Native frame accounting changed')
        wires = [bytes(chunk) for chunk in arm['chunks']]
        for seq, wire in enumerate(wires):
            require(21 <= len(wire) <= 1295 and wire[:8] == b'GLA1\x01\x02\0\0'
                    and int.from_bytes(wire[8:12], 'little') == 48000
                    and int.from_bytes(wire[12:16], 'little') == seq
                    and int.from_bytes(wire[16:20], 'little') == frames, 'Native wire header changed')
            expected_journal.extend([{'event': 'attempt', 'arm': index, 'packet': seq, 'frames': frames},
                {'event': 'packet', 'arm': index, 'packet': seq, 'frames': frames,
                 'bytes': len(wire), 'sha256': digest(wire)}])
        require(p['slot'] == index + 1 and p['packet_frames'] == frames and p['packets'] == count
                and p['wire_sha256'] == digest(b''.join(wires)) and p['status'] == 'measured'
                and p['passed'] is True and p['page_errors'] == [], 'Browser packet binding changed')
        m = p['metrics'];s = m['snapshot']
        require(len(m['sends']) == len(m['outputs']) == len(m['schedules']) == count
                and m['errors'] == [] and m['schedules'] == [frames] * count, 'Browser output cardinality changed')
        for seq, (sent, output) in enumerate(zip(m['sends'], m['outputs'])):
            timestamp = seq * frames * 1000000 // 48000
            require(sent['sequence'] == seq and sent['frames'] == frames and sent['timestamp'] == timestamp
                    and sent['late_ms'] >= 0 and abs(output['timestamp'] - timestamp) <= 1
                    and output['frames'] == frames and output['sample_rate'] == 48000 and output['channels'] == 2,
                    'Native packet/browser output mapping changed')
        require(s['received'] == s['scheduled'] == count and s['dropped'] == s['pendingDecodes'] == 0
                and s['enabled'] and s['codec'] == 'opus' and s['sources'] == 0 and s['queuedSeconds'] == 0,
                'Pending, missing or dropped output')
        require(p['stopped']['enabled'] is False and p['stopped']['contextState'] == 'closed'
                and p['stopped']['sources'] == 0, 'Player cleanup unconfirmed')
        r = p['resource_after']
        require(r['pids.max'] == 256 and r['pids.events']['max'] == 0 and r['memory.max'] == 2147483648
                and all(r['memory.events'][key] == 0 for key in ['max', 'oom', 'oom_kill']), 'Resource admission failed')
        packets += count
    expected_journal.append({'event': 'complete'})
    require(journal == expected_journal and len(journal) == 106 and packets == 52, 'Durable journal incomplete')
    require(native['arms'][0]['chunks'] == native['arms'][3]['chunks']
            and native['arms'][1]['chunks'] == native['arms'][2]['chunks'], 'Repeated native packets differ')
    print('Verified 52 native packets -> 52 browser outputs/schedules, including 18 outputs of 1920 frames. '
          'This proves this Chromium accounting fixture, not 40 ms production latency or performance.')


if __name__ == '__main__':
    main()
