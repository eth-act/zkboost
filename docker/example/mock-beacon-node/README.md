# Mock Beacon Node with zkboost

This example runs zkboost with two mock zkVM backends against a local Kurtosis testnet. `mock-beacon-node` takes the place of the beacon node.

The Kurtosis testnet has three Geth/Lighthouse participants that produce blocks. Docker Compose runs a separate Geth. Only zkboost uses its Engine API.

## Proof flow

- The mock beacon node follows the canonical head of the Lighthouse of the first participant. It sends the Gloas payloads to zkboost as `engine_newPayloadV5`.
- zkboost forwards the payload to the dedicated Geth and proves it with the mocks.
- zkboost submits the signed proofs to the mock beacon node, which verifies them.

## Start

1. Run `./docker/scripts/start_local_testnet.sh -n docker/example/mock-beacon-node/network_params.yaml`.
2. Run `docker compose -f docker/example/mock-beacon-node/docker-compose.yml up -d --build`.

### Testnet settings

- The scripts need Docker, Kurtosis, and `yq`.
- The enclave name defaults to `local-testnet`. To change it, export `ENCLAVE_NAME` before you run the scripts and Compose.
- The genesis is saved to `docker/scripts/genesis-${ENCLAVE_NAME}.json` and mounted automatically.
- Compose services share the project-scoped `zkboost` network. Geth and the mock beacon node also join the enclave network `kt-${ENCLAVE_NAME}`.
- After you recreate the testnet, also recreate the Compose Geth container. Its chain data lives in the container.

### Dedicated Geth

- Uses the enclave genesis and syncs missing history from the first testnet Geth.
- `start_geth.sh` takes the peer RPC URL, fetches its enode, and writes the peer config and testnet JWT secret.
- Needs no fixed node key. Its Engine API has no published host port.
- All Kurtosis Lighthouse nodes keep running.

### Head tracking and sync

The mock beacon node refreshes the canonical head on every SSE head event and every two seconds. The periodic refresh also recovers missed events after a reconnect.

1. It announces a missing parent with `engine_forkchoiceUpdatedV4`, so Geth syncs the history from the peer.
2. It executes the new payload through zkboost, then advances forkchoice.
3. It waits for the proofs of the block and processes the next heads meanwhile.

If the parent is still syncing, the next refresh retries.

## Dashboards

The zkboost dashboard is at http://localhost:3000/dashboard.

## Stop

1. Run `docker compose -f docker/example/mock-beacon-node/docker-compose.yml down`.
2. Run `./docker/scripts/stop_local_testnet.sh`.
