#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <peer-rpc-url>" >&2
  exit 1
fi

# Kurtosis exposes admin over its internal RPC. Retry while the peer starts.
# The Geth console prints the enode as a quoted string, ready for TOML.
while :; do
  if peer_enode=$(geth --exec admin.nodeInfo.enode attach "$1") &&
    printf '%s\n' "$peer_enode" | grep -Eq '^"enode://[0-9a-fA-F]{128}@[a-zA-Z0-9.:-]+(\?discport=[0-9]+)?"$'; then
    break
  fi
  echo "Waiting for the peer enode at $1" >&2
  sleep 2
done

printf '[Node.P2P]\nStaticNodes = [%s]\n' "$peer_enode" > /geth.toml

# Shared Kurtosis testnet JWT secret.
printf '%s\n' dc49981516e8e72b401a63e6405495a32dafc3939b5d6d83cc319ac0388bca1b > /jwtsecret

exec geth \
  --config=/geth.toml \
  --override.genesis=/genesis.json \
  --datadir=/data \
  --syncmode=full \
  --nodiscover \
  --maxpeers=1 \
  --authrpc.addr=0.0.0.0 \
  --authrpc.vhosts='*' \
  --authrpc.jwtsecret=/jwtsecret
