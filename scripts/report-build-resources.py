#!/usr/bin/env python3
"""Bounded Linux build diagnostics without command lines or environment values."""

from collections import Counter
import json
import os
from pathlib import Path

counts = Counter()
truncated = False
for index, process in enumerate(path for path in Path('/proc').iterdir() if path.name.isdigit()):
    if index == 512:
        truncated = True
        break
    try:
        with (process / 'status').open() as source:
            fields = dict(line.split(':', 1) for line in source.read(16384).splitlines() if ':' in line)
        counts[(fields['Name'].strip(), fields['State'].strip().split()[0])] += 1
    except (FileNotFoundError, ProcessLookupError):
        continue
limits = {}
for name in ('pids.current', 'pids.max', 'pids.events', 'memory.peak', 'memory.events', 'cpu.max'):
    with (Path('/sys/fs/cgroup') / name).open() as source:
        limits[name] = source.read(4096).strip()
print('[build-resources] ' + json.dumps({
    'cgroup': limits, 'cpus': sorted(os.sched_getaffinity(0)),
    'processes': [{'name': name, 'state': state, 'count': count}
                  for (name, state), count in counts.most_common(32)],
    'truncated': truncated or len(counts) > 32,
}), flush=True)
