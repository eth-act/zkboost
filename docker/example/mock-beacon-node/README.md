# Mock Beacon Node with zkboost

This example runs zkboost with two mock zkVM backends against a local Kurtosis testnet. `mock-beacon-node` takes the place of the beacon node.

The Kurtosis testnet has three Geth/Lighthouse participants that produce blocks and a fourth Lighthouse without an execution client. Docker Compose runs a separate Geth. Only zkboost uses its Engine API.

## Proof flow

- The execution endpoint of the fourth Lighthouse is the mock beacon node. The mock forwards every Engine API request to zkboost with the JWT of the Lighthouse unchanged and answers with the response of zkboost.
- zkboost forwards the request to the dedicated Geth. For a valid `engine_newPayloadV5` payload it obtains the witness and proves the payload with the mocks.
- zkboost submits the signed proofs to the mock beacon node. The mock looks up the beacon block of every envelope at the fourth Lighthouse and verifies the signature and the proof. Every other beacon API request goes to that Lighthouse.

## Start

1. Run `./docker/scripts/start_local_testnet.sh -n docker/example/mock-beacon-node/network_params.yaml`.
2. Run `docker compose -f docker/example/mock-beacon-node/docker-compose.yml up -d --build`.

### Testnet settings

- The scripts need Docker, Kurtosis, and `yq`.
- The enclave name defaults to `local-testnet`. To change it, export `ENCLAVE_NAME` before you run the scripts and Compose.
- The genesis is saved to `docker/scripts/genesis-${ENCLAVE_NAME}.json` and mounted automatically.
- Compose services share the project-scoped `zkboost` network. Geth and the mock beacon node also join the enclave network `kt-${ENCLAVE_NAME}`, where the fourth Lighthouse reaches the Engine API of the mock at `mock-beacon-node:8551`.
- After you recreate the testnet, also recreate the Compose Geth container. Its chain data lives in the container.

### Dedicated Geth

- Uses the enclave genesis and syncs missing history from the first testnet Geth.
- `start_geth.sh` takes the peer RPC URL, fetches its enode, and writes the peer config and testnet JWT secret.
- Needs no fixed node key. Its Engine API has no published host port.
- All Kurtosis Lighthouse nodes keep running.

## Dashboards

The zkboost dashboard is at http://localhost:3000/dashboard.

## Stop

1. Run `docker compose -f docker/example/mock-beacon-node/docker-compose.yml down`.
2. Run `./docker/scripts/stop_local_testnet.sh`.
