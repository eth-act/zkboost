# Observability Stack with zkboost

This example runs zkboost with three mock zkVM backends, `mock-beacon-node`, Prometheus, Tempo, and Grafana against a local Kurtosis testnet.

The Kurtosis testnet has three Geth/Lighthouse participants that produce blocks. Docker Compose runs a separate Geth whose Engine API is used only by zkboost.

## Proof flow

- The mock beacon node follows the canonical head of the first participant’s Lighthouse and sends its Gloas payloads to zkboost as `engine_newPayloadV5`.
- zkboost forwards the payload to the dedicated Geth, proves it with the mocks, and exports its spans to Tempo.
- zkboost submits the signed proofs to the mock beacon node, which verifies them.

The `reth-sp1` mock has `mock_failure = true`. It shows the failure path and makes the mock beacon node wait its proof timeout for every block.

## Start

1. Run `./docker/scripts/start_local_testnet.sh -n docker/example/observability/network_params.yaml`.
2. Run `docker compose -f docker/example/observability/docker-compose.yml up -d --build`.

### Testnet settings

- **Requirements:** Docker, Kurtosis, and `yq`.
- **Enclave:** defaults to `local-testnet`. To change it, export `ENCLAVE_NAME` before running the scripts and Compose.
- **Genesis:** saved to `docker/scripts/genesis-${ENCLAVE_NAME}.json` and mounted automatically.
- **Networks:** `zkboost` is scoped to the Compose project; services that access the testnet also join `kt-${ENCLAVE_NAME}`.
- **After recreating the testnet:** recreate the Compose Geth container too. Its chain data lives in the container.

### Dedicated Geth

- Uses the enclave genesis and syncs missing history from the first testnet Geth.
- `start_geth.sh` takes the peer RPC URL, fetches its enode, and writes the peer config and testnet JWT secret.
- Needs no fixed node key. Its Engine API has no published host port.
- All Kurtosis Lighthouse nodes keep running.

### Head tracking and sync

The mock beacon node refreshes the canonical head on SSE notifications and every two seconds, including after reconnecting.

1. Announce missing parents and send `engine_forkchoiceUpdatedV4` to sync history from the peer.
2. Execute the new payload through zkboost, then advance forkchoice.
3. Verify proofs independently while processing further execution updates in order.

Catch-up retries on subsequent refreshes.

## Dashboards

| Service    | URL                             | Credentials   |
| ---------- | ------------------------------- | ------------- |
| zkboost    | http://localhost:3000/dashboard | -             |
| Prometheus | http://localhost:9090           | -             |
| Grafana    | http://localhost:3002           | admin / admin |

The zkboost dashboard of Grafana is provisioned under Dashboards.

## Stop

1. Run `docker compose -f docker/example/observability/docker-compose.yml down`.
2. Run `./docker/scripts/stop_local_testnet.sh`.
