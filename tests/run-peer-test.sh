#!/bin/sh
# Runs the ignored qso test against tests/peer.py in a private pair of network
# namespaces joined by a veth link (no root needed, nothing touches the host).
set -e
cd "$(dirname "$0")/.."
bin=$(cargo test --lib --no-run --message-format=json 2>/dev/null | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | head -1)
exec unshare -rn --fork sh -c '
  unshare -n sleep 60 & B=$!; sleep 0.3
  ip link add v0 type veth peer name v1 netns $B
  ip addr add 10.9.0.1/24 dev v0; ip link set v0 up; ip link set lo up
  nsenter -t $B -n sh -c "ip addr add 10.9.0.2/24 dev v1; ip link set v1 up; ip link set lo up"
  nsenter -t $B -n python3 -I tests/peer.py 10.9.0.2 10.9.0.1 > /tmp/peer-report.$$ &
  P=$!; sleep 0.5
  PEER_IP=10.9.0.2 "$1" --ignored --nocapture qso_with_simulated_peer; rc=$?
  wait $P; cat /tmp/peer-report.$$; rm -f /tmp/peer-report.$$; kill $B
  exit $rc' sh "$bin"
