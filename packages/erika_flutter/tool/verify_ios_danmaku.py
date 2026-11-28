#!/usr/bin/env python3
"""Check the iOS smoke app through its debug VM and native presenter trace."""
import argparse
import json
import re
import time
import urllib.parse
import urllib.request
from pathlib import Path


class Smoke:
    def __init__(self, url, trace):
        self.url = url.rstrip('/') + '/'
        self.trace = trace
        self.isolate = next(i['id'] for i in self.request('getVM')['isolates']
                            if i['name'] == 'main')

    def request(self, method, **params):
        query = urllib.parse.urlencode({
            k: str(v).lower() if isinstance(v, bool) else v
            for k, v in params.items()
        })
        with urllib.request.urlopen(self.url + method + '?' + query,
                                    timeout=10) as response:
            value = json.load(response)
        if 'error' in value:
            raise RuntimeError(value['error'])
        return value['result']

    def call(self, **params):
        method = 'control' if params else 'snapshot'
        return self.request('ext.erikaSmoke.' + method,
                            isolateId=self.isolate, **params)

    def frame(self):
        with self.trace.open('rb') as stream:
            stream.seek(max(0, self.trace.stat().st_size - 65536))
            lines = stream.read().decode(errors='replace').splitlines()
        line = next(line for line in reversed(lines)
                    if '[erika-presenter-trace] stage=render_tick ' in line)
        return dict(re.findall(r'(\w+)=([^ ]+)', line))

    def settled(self, position, items=True):
        deadline = time.monotonic() + 5
        while True:
            frame = self.frame()
            if (abs(float(frame['media']) - position) < .002
                    and (int(frame['danmaku_items']) > 0) == items):
                snapshot = self.call()
                assert not snapshot['errors'], snapshot
                assert all(snapshot['stats'][key] == 0 for key in
                           ['renderFailures', 'audioFailures', 'importFailures'])
                return {'media': float(frame['media']),
                        'generation': int(frame['gen']),
                        'glyphs': int(frame['danmaku_items'])}
            if time.monotonic() >= deadline:
                raise AssertionError((position, items, frame))
            time.sleep(.05)


def verify(smoke):
    assert smoke.call()['ready']
    evidence = []
    for position in [20, 8, 8.1, 8]:
        smoke.call(playing=False, position=position)
        evidence.append({'case': f'paused seek {position}',
                         'frame': smoke.settled(position)})
    before = smoke.settled(8)
    time.sleep(1)
    assert smoke.settled(8) == before
    evidence.append({'case': 'pause stable', 'passed': True})
    smoke.call(offset=2)
    time.sleep(.7)
    evidence.append({'case': 'offset preserves playback clock',
                     'frame': smoke.settled(8)})
    smoke.call(offset=0, visible=False, position=15)
    smoke.settled(15, items=False)
    smoke.call(visible=True)
    evidence.append({'case': 'hidden seek then show',
                     'frame': smoke.settled(15)})
    smoke.call(position=60)
    smoke.settled(60, items=False)
    smoke.call(position=5)
    evidence.append({'case': 'empty segment then return',
                     'frame': smoke.settled(5)})
    for position in [24, 2, 17, 7.05]:
        smoke.call(position=position)
    evidence.append({'case': 'consecutive seeks last wins',
                     'frame': smoke.settled(7.05)})
    for rate in [.5, 1, 2]:
        smoke.call(playing=False, position=10, rate=rate)
        smoke.settled(10)
        smoke.call(playing=True)
        time.sleep(.5)
        start = float(smoke.frame()['media'])
        wall = time.monotonic()
        time.sleep(1.5)
        movement = float(smoke.frame()['media']) - start
        elapsed = time.monotonic() - wall
        assert abs(movement - rate * elapsed) < .25, (rate, movement, elapsed)
        evidence.append({'case': f'rate {rate}', 'wall_seconds': elapsed,
                         'media_advance': movement})
    smoke.call(playing=False, position=8, rate=1)
    smoke.settled(8)
    return evidence


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('vm_url')
    parser.add_argument('--trace', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = verify(Smoke(args.vm_url, args.trace))
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(f'PASS: {len(result)} iOS playback cases')
