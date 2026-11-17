#!/usr/bin/env python3
"""Admit a shell gate and keep its cache-volume lock until the process exits."""

import os
import sys

from build_tools import admission, environment

if len(sys.argv) < 2:
    raise SystemExit('usage: with-build-cache.py <command> [arguments...]')
env = environment()
with admission() as lock:
    os.set_inheritable(lock.fileno(), True)
    os.execvpe(sys.argv[1], sys.argv[1:], env)
