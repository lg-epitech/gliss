"""Recompute the published terminal table and retain motion exclusion counts."""
import argparse
import json
import math
from pathlib import Path


def number(value):
    assert type(value) in (int, float) and math.isfinite(value) and value >= 0
    return value


def quantile(values, fraction):
    return sorted(values)[math.ceil(fraction*len(values))-1]


def summarize(value):
    assert value['schema'] == 'gliss-public-reference-v1'
    terminal = value['terminal_trials']
    assert len(terminal) == 20 and len({row['id'] for row in terminal}) == 20
    table = []
    for network in ('loopback', 'wan'):
        for candidate in ('deflate', 'adaptive', 'batch', 'h264', 'guacamole'):
            rows = sorted((row for row in terminal if row['candidate'] == candidate and row['network'] == network),
                          key=lambda row: row['repetition'])
            assert len(rows) == 2 and [row['repetition'] for row in rows] == [0, 1]
            latencies, rates, service_cpu, browser_cpu = [], [], [], []
            for row in rows:
                assert row['status'] == 'ok' and row['eligible'] is True and len(row['phases']) == 5
                total_bytes = total_time = 0
                for phase in row['phases']:
                    total_bytes += number(phase['down_bytes'])
                    total_time += number(phase['duration_s'])
                    samples = phase['input_latency_ms']
                    if phase['name'] == 'root_color':
                        assert len(samples) == 10
                        latencies.extend(number(sample) for sample in samples)
                    else:
                        assert samples == []
                assert total_time > 0
                rates.append(total_bytes*8/total_time/1000000)
                service_cpu.append(round(sum(number(p['service_cpu_s']) for p in row['phases']), 3))
                browser_cpu.append(round(sum(number(p['browser_cpu_s']) for p in row['phases']), 3))
                quality = row['text_quality']
                number(quality['mean_absolute_rgb_error'])
                assert 0 <= number(quality['exact_mismatch_fraction']) <= 1
            assert len(latencies) == 20
            table.append(dict(network=network, candidate=candidate, responses=len(latencies),
                p50_ms=round(quantile(latencies,.5),1), p95_ms=round(quantile(latencies,.95),1),
                downstream_mbps=[round(rate,3) for rate in rates],
                service_cpu_seconds=service_cpu, browser_cpu_seconds=browser_cpu))
    moving = value['motion_trials']
    assert len(moving) == 36 and len({row['id'] for row in moving}) == 36
    assert all(row['cleanup_proven'] is True for row in moving)
    admitted = [row for row in moving if row['eligible']]
    assert len(admitted) == 24 and sum(row['network'] == 'control' for row in admitted) == 18
    assert sum(row['network'] == 'wan-loss' for row in admitted) == 6
    assert all(row['source_rate_eligible'] for row in admitted)
    return dict(terminal_responses=sum(row['responses'] for row in table), terminal=table,
                motion_planned=len(moving), motion_eligible=len(admitted), motion_excluded=len(moving)-len(admitted))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', nargs='?', type=Path, default=Path(__file__).with_name('reference.json'))
    args = parser.parse_args()
    print(json.dumps(summarize(json.loads(args.input.read_bytes())), indent=2, allow_nan=False))
