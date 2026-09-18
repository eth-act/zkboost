# Local Testnet with zkboost

This example runs a local Kurtosis testnet with geth and lighthouse. Gloas is active from genesis. zkboost runs beside it with two Ere GPU provers: Ethrex/OpenVM on GPU 0 and Reth/Zisk on GPU 1. A lighthouse built from the `optional-proofs-gloas` branch of [eth-act/lighthouse](https://github.com/eth-act/lighthouse) uses zkboost as its Engine API endpoint.

zkboost forwards every request to a dedicated Geth started by Docker Compose. It obtains the execution witness of each valid payload and requests a proof.

```mermaid
sequenceDiagram
    participant CL as lighthouse <br> (Kurtosis)
    participant zkboost as zkboost <br> server
    participant EL as geth <br> (Docker Compose)
    participant Ere as Ere <br> server(s)
    CL->>zkboost: engine_newPayloadV5 <br>
    zkboost->>EL: engine_newPayloadWithWitnessV5
    EL->>zkboost: payload status + witness
    zkboost->>CL: payload status
    zkboost->>Ere: Request proof
    Ere->>zkboost:
    zkboost->>CL: GET headers and blocks
    zkboost->>CL: POST /eth/v1/beacon/execution_proofs
```

## Testnet layout

| Participants | Role                                                                                      |
| ------------ | ----------------------------------------------------------------------------------------- |
| First two    | Geth and Lighthouse block producers with two thirds of the stake.                         |
| Third        | eth-act Lighthouse. It follows gossip and executes through zkboost and the Compose Geth.  |
| Fourth       | eth-act Lighthouse with a proof engine, no EL or execution endpoint, and zero validators. |

The third Lighthouse uses the proof-engine verifying keys in `network_params.yaml`. zkboost signs proofs as validator 256 with the keystore in `docker/example/testnet/validator-keys` and submits them to this Lighthouse.

### Networking

Compose services share the project-scoped `zkboost` network. Geth and zkboost also join the enclave network.

| Connection            | Address                |
| --------------------- | ---------------------- |
| Lighthouse to zkboost | `zkboost:3000`         |
| zkboost to Geth       | `geth:8551`            |
| zkboost to Lighthouse | `cl-3-lighthouse:4000` |

### Dedicated Geth

- Uses the enclave genesis and syncs missing history from the first testnet Geth.
- `start_geth.sh` takes the peer RPC URL, fetches its enode, and writes the peer config and testnet JWT secret.
- Needs no fixed node key. Its Engine API has no published host port.
- All Kurtosis Lighthouse nodes keep running.

## Installation

1. Install [Docker](https://docs.docker.com/get-docker/). Run `sudo docker run hello-world` to check the installation.

1. Install [Kurtosis](https://docs.kurtosis.com/install/). Run `kurtosis version` to check the installation.

1. Install [`yq`](https://github.com/mikefarah/yq). On Ubuntu, `snap install yq` installs it.

## Start local testnet

Run every command from the repository root.

The start script builds `lighthouse:eth-act-optional-proofs-gloas` from the eth-act branch, starts the enclave, and saves the enclave genesis for the dedicated Geth. The image build takes several minutes. Pass `-b false` to reuse an existing image.

- The enclave name defaults to `local-testnet`. To change it, export `ENCLAVE_NAME` before you run the scripts and Compose.
- The genesis is saved to `docker/scripts/genesis-${ENCLAVE_NAME}.json` and mounted automatically.

```
./docker/scripts/start_local_testnet.sh
```

## Start zkboost and provers

```
docker compose -f ./docker/example/testnet/docker-compose.yml up -d
```

| Prover          | GPU device | Proof type |
| --------------- | ---------- | ---------- |
| `ethrex-openvm` | `0`        | `1`        |
| `reth-zisk`     | `1`        | `6`        |

### Check progress

- The dashboard is at http://localhost:3000/dashboard.
- zkboost logs `received new payload` and then `proof dispatched` for each payload. The third Lighthouse reports `is_optimistic: false` and `el_offline: false` at `/eth/v1/node/syncing`. Its Gloas status log can show `exec_hash: "n/a"`. Use the two checks below for proof verification.
- zkboost logs `proof submitted` with `proof_type=ethrex-openvm` and `proof_type=reth-zisk` after Lighthouse accepts the signed proofs. A `proof submission failed` log includes the Lighthouse reason.
- Query `GET /eth/v1/beacon/execution_proofs/{block_id}` on the third Lighthouse with the root or slot of the proven block. Look for both proof types `"1"` and `"6"` for the same block to confirm Lighthouse cached both proofs. Query soon after the submission, because the cache is bounded. The `head` block can be newer than the proven block.

For example (replace `3` with a proven slot in your run):

```bash
SLOT="3"
BEACON_API=$(kurtosis port print "${ENCLAVE_NAME:-local-testnet}" cl-3-lighthouse http)
curl -fsS "$BEACON_API/eth/v1/beacon/execution_proofs/$SLOT" \
  | yq -p=json '.data[] | {"proof_type": .message.proof_type, "validator_index": .validator_index, "beacon_block_root": .message.beacon_block_root}'
```

Query the fourth node as well to check proof gossip:

```bash
BEACON_API=$(kurtosis port print "${ENCLAVE_NAME:-local-testnet}" cl-4-lighthouse http)
curl -fsS "$BEACON_API/eth/v1/node/syncing"
curl -fsS "$BEACON_API/eth/v1/beacon/execution_proofs/$SLOT" \
  | yq -p=json '.data[] | {"proof_type": .message.proof_type, "validator_index": .validator_index, "beacon_block_root": .message.beacon_block_root}'
```

## Stop

1. Run `docker compose -f ./docker/example/testnet/docker-compose.yml down`.
2. Run `./docker/scripts/stop_local_testnet.sh`.
