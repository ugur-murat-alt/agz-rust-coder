#!/bin/sh
set -eu

pid_file=$1
/bin/sh -c 'trap "" TERM; exec </dev/null >/dev/null 2>&1; while :; do sleep 60; done' &
child_pid=$!
printf '%s\n' "$child_pid" > "$pid_file"
trap 'exit 0' TERM
wait "$child_pid"
