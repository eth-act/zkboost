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

## Proof observer with a GPU

The commented lines of the Compose file, the zkboost config, and `network_params.yaml` turn the example into the proof flow of the testnet example on one GPU. An Ere server proves the mock OpenVM guest of `mock-guest`, which reveals its input unchanged, for both proof types. The fourth Lighthouse comes from eth-act and gossips the proofs to a fifth Lighthouse, the proof observer, which imports every payload after its proofs and has no execution client.

1. In `docker-compose.yml`, uncomment the `openvm` service, the `openvm` dependency of zkboost, and the `kurtosis` network of zkboost.
2. In `zkboost/config.toml`, uncomment the `endpoint` of both mocks, and replace the `cl_beacon_endpoint` of the mock beacon node with the commented one.
3. In `network_params.yaml`, replace the image and the parameters of the fourth Lighthouse with the commented ones, and uncomment the proof observer, `genesis_delay`, and `extra_files`.
4. Start the testnet as above. The start script builds the eth-act Lighthouse image, which takes several minutes. Run Compose as soon as the start script prints `Started!`.

zkboost then posts the proofs to the fourth Lighthouse, and the mock beacon node stays idle. The observer reports the head of the testnet at `/eth/v1/node/syncing` and lists both proof types at `/eth/v1/beacon/execution_proofs/{slot}`:

```bash
BEACON_API=$(kurtosis port print "${ENCLAVE_NAME:-local-testnet}" cl-5-lighthouse http)
curl -fsS "$BEACON_API/eth/v1/node/syncing"
curl -fsS "$BEACON_API/eth/v1/beacon/execution_proofs/head" | yq -p=json '[.data[].message.proof_type]'
```

`mock-guest` holds one guest per zkVM as an assembly source, a linker script, the linked ELF, and the verifying key. This example uses the OpenVM guest. The build commands are in the header of each source. The `keygen` command of the ere-server of the zkVM writes the key:

```bash
docker run --rm -v $PWD/docker/example/mock-beacon-node/mock-guest:/mock-guest ghcr.io/eth-act/ere/ere-server-openvm:0.17.0 --elf-path /mock-guest/openvm.elf keygen --program-vk-path /mock-guest/openvm.vk
```

## Dashboards

The zkboost dashboard is at http://localhost:3000/dashboard.

## Stop

1. Run `docker compose -f docker/example/mock-beacon-node/docker-compose.yml down`.
2. Run `./docker/scripts/stop_local_testnet.sh`.
