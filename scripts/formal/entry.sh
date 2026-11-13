#!/usr/bin/env bash
set -euo pipefail

# Check the actual kernel boundary before starting installers or verifiers.
# Environment variables and a small ps RSS are not evidence of containment.
[[ "$(uname -s)" == Linux && -f /sys/fs/cgroup/cgroup.controllers ]] || {
  echo 'Formal verification requires Linux cgroup v2 containment.' >&2; exit 125;
}
memory="$(cat /sys/fs/cgroup/memory.max)"
pids="$(cat /sys/fs/cgroup/pids.max)"
read -r quota period < /sys/fs/cgroup/cpu.max
[[ "$memory" =~ ^[0-9]+$ && "$memory" -le 4294967296 && "$memory" -gt 0 &&
   "$(cat /sys/fs/cgroup/memory.swap.max)" == 0 &&
   "$pids" =~ ^[0-9]+$ && "$pids" -le 128 && "$pids" -gt 0 &&
   "$quota" =~ ^[0-9]+$ && "$quota" -le "$((period * 2))" ]] || {
  echo 'Missing or excessive memory, swap, process, or CPU limit; refusing to run.' >&2
  exit 125
}
printf 'Enforced limits: memory=%s bytes, swap=0, pids=%s, cpu=%s/%s\n' "$memory" "$pids" "$quota" "$period"
available_kib="$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)"
[[ "$available_kib" =~ ^[0-9]+$ && "$((available_kib * 1024))" -ge "$((memory + 1073741824))" ]] || {
  echo 'Insufficient available VM memory for this run plus 1 GiB headroom.' >&2; exit 125;
}
deadline="$1"
shift
[[ "$deadline" =~ ^[0-9]+$ && "$deadline" -gt 0 && "$deadline" -le 900 ]] || exit 125
# This deadline survives loss/SIGKILL of the host client. When PID 1 exits,
# Docker tears down the PID namespace, including detached solver descendants.
exec timeout --signal=TERM --kill-after=5 "${deadline}s" "$@"
