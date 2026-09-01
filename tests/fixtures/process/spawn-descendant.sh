#!/bin/sh
set -eu

pid_file=$1
sleep 60 &
child_pid=$!
printf '%s\n' "$child_pid" > "$pid_file"
printf '\033[31mstarted descendant\033[0m\n'
printf '\033[33mchild is waiting\033[0m\n' >&2
wait "$child_pid"
