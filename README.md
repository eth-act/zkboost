
<p align="center">
  <img src="assets/logo.png" width="270" alt="zkboost logo" />
</p>

<p align="center"><b>Engine API Proxy That Proves Execution Payloads</b></p>

zkboost is an Engine API proxy between a consensus client (CL) and its execution client (EL). It forwards every Engine API request to the EL. For each valid `engine_newPayloadV5` payload it generates zkVM proofs of the stateless block execution. It signs them as a validator and submits them to a beacon node as EIP-8025 execution proof envelopes.

## Table of Contents

- [Table of Contents](#table-of-contents)
- [Quick Start](#quick-start)
- [Manual Build](#manual-build)
  - [Prerequisites](#prerequisites)
- [Configuration](#configuration)
- [Engine API](#engine-api)
- [Proof Submission](#proof-submission)
- [Mock Beacon Node](#mock-beacon-node)
- [Observability](#observability)
  - [Docker Compose with Grafana](#docker-compose-with-grafana)
  - [Available Metrics](#available-metrics)
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Quick Start

See [docker/example/testnet](docker/example/testnet) for a Docker Compose setup that runs zkboost with real Ere backends on a local testnet. See [docker/example/mock-beacon-node](docker/example/mock-beacon-node) for mock backends without a GPU. There `mock-beacon-node` stands in for the beacon node.

## Manual Build

### Prerequisites

* **Rust** ≥ 1.94.1

```bash
# 1. Clone
git clone https://github.com/eth-act/zkboost.git && cd zkboost

# 2. Build
cargo build --release

# 3. Run
./target/release/zkboost --config <config-path>
```

## Configuration

zkboost reads a TOML file passed with `--config <path>`. The annotated example below shows every option.

```toml
# Engine API server port (default: 3000)
port = 3000

# Ethereum execution layer Engine API endpoint (required).
# Every Engine API request is forwarded to this endpoint.
el_engine_endpoint = "http://localhost:8551"

# Beacon API endpoint that receives every proof at POST /eth/v1/beacon/execution_proofs (required).
cl_beacon_endpoint = "http://localhost:4000"

# EIP-2335 keystore of the validator that signs the proofs and its plain text password (required).
# The files are the ones a lighthouse validator client reads from its validators and secrets directories.
# The validator index is read from the beacon node.
validator_keystore_path = "/validator-keys/keys/<pubkey>/voting-keystore.json"
validator_keystore_password_path = "/validator-keys/secrets/<pubkey>"

# Built-in dashboard served at /dashboard
[dashboard]
# enabled = false
# retention = 256

# External Ere server (calls a remote ere-server via HTTP)
[[zkvm]]
kind = "ere"
proof_type = "ethrex-zisk"

# Timeout for proof generation in seconds (default: 12)
# proof_timeout_secs = 12

# Endpoint of the Ere server
endpoint = "http://ere-server:3000"

# External proving cluster (ZisK and OpenVM proof types)
[[zkvm]]
kind = "cluster"
proof_type = "reth-zisk"
endpoint = "http://zisk-cluster:50051"
elf_url = "https://example.com/stateless-validator-reth-zisk.elf"
# elf_path = "/path/to/program.elf"   # mutually exclusive with elf_url

# An OpenVM cluster is the manager of the han0110/axiom-edge fork. Its loadout must hold the guest named after the ELF.
[[zkvm]]
kind = "cluster"
proof_type = "reth-openvm"
endpoint = "http://openvm-cluster:3000"
elf_url = "https://example.com/stateless-validator-reth-openvm.elf"

# Mock zkVMs (in-process, for testing without Docker/GPU).
# The mock sleeps for the simulated proving time and returns the proof bytes `MOCK`.

# Fixed proving time (default)
[[zkvm]]
kind = "mock"
proof_type = "reth-sp1"
mock_proving_time = { kind = "constant", ms = 6000 }

# Random proving time uniformly sampled from [min_ms, max_ms]
[[zkvm]]
kind = "mock"
proof_type = "ethrex-sp1"
mock_proving_time = { kind = "random", min_ms = 2000, max_ms = 8000 }

# Proving time proportional to block gas (ms_per_mgas * gas_used / 1000_000)
[[zkvm]]
kind = "mock"
proof_type = "ethrex-openvm"
mock_proving_time = { kind = "linear", ms_per_mgas = 300 }

# Simulated failure (always returns a proving error)
[[zkvm]]
kind = "mock"
proof_type = "reth-openvm"
mock_failure = true
```

Available proof types:

| Name            | EL       | zkVM   |
| --------------- | -------- | ------ |
| `ethrex-openvm` | `ethrex` | OpenVM |
| `ethrex-sp1`    | `ethrex` | SP1    |
| `ethrex-zisk`   | `ethrex` | ZisK   |
| `reth-openvm`   | `reth`   | OpenVM |
| `reth-sp1`      | `reth`   | SP1    |
| `reth-zisk`     | `reth`   | ZisK   |
| `zesu-zisk`     | `zesu`   | ZisK   |

## Engine API

zkboost serves the Engine API as JSON-RPC at `POST /`. The CL uses zkboost as its Engine API endpoint in place of the EL.

- Every request is forwarded byte-for-byte to `el_engine_endpoint`. The EL response is returned to the CL.
- The `Authorization` JWT header of the CL is passed through unchanged. The EL validates the JWT. zkboost does not validate it.
- zkboost takes the chain id from `DEPOSIT_CHAIN_ID` of the beacon node spec.
- `engine_newPayloadV5` is intercepted. zkboost sends it to the EL as `engine_newPayloadWithWitnessV5`, which returns the execution witness in the payload status.
- zkboost removes the `witness` field from the payload status before it answers the CL.
- When the payload status is `VALID`, zkboost builds the stateless input from the payload, the witness, and the chain id. It then dispatches proving to every configured zkVM backend in the background.
- Every payload is proven under the Amsterdam rules. `engine_newPayloadV1` to `engine_newPayloadV4` are forwarded without proving.
- A payload imported again after a reorg is submitted with the cached proof, not proven twice. zkboost keeps the proofs of the last 64 payloads per proof type, two epochs.

The following endpoints are also available:

| Method | Endpoint            | Purpose                             |
| ------ | ------------------- | ----------------------------------- |
| `GET`  | `/health`           | Health check                        |
| `GET`  | `/metrics`          | Prometheus metrics                  |
| `GET`  | `/dashboard`        | Built-in dashboard UI (when enabled) |
| `GET`  | `/dashboard/state`  | Dashboard state JSON snapshot       |
| `GET`  | `/dashboard/events` | Dashboard SSE event stream          |

## Proof Submission

zkboost delivers every generated proof to the beacon node at `cl_beacon_endpoint` as an EIP-8025 `SignedExecutionProofEnvelope`.

zkboost reads these routes of the beacon API.

| Route                                                | Reads                                           | How often                  |
| ---------------------------------------------------- | ----------------------------------------------- | -------------------------- |
| `GET /eth/v1/config/spec`                            | The chain spec                                  | At startup                 |
| `GET /eth/v1/beacon/genesis`                         | The genesis validators root                     | At startup                 |
| `GET /eth/v1/beacon/states/head/validators/{pubkey}` | The index of the signing validator              | At startup                 |
| `GET /eth/v1/events?topics=execution_payload`        | The beacon block root of every imported payload | Streamed                   |
| `GET /eth/v1/beacon/headers?parent_root=`            | The children of the parent beacon block root    | Per proof without an event |
| `GET /eth/v2/beacon/blocks/{root}`                   | The payload bid of a listed child               | Per listed child           |

zkboost forwards payloads without proving until the beacon node answers the three startup routes, and logs one warning per attempt.

Every proof follows these steps.

1. zkboost takes the beacon block root from the `execution_payload` event of the block hash and the slot of the payload. The beacon node sends that event when it imports the payload envelope.
2. Without such an event, zkboost lists the children of the parent beacon block root. It reads the payload bid of every listed child and takes the child whose bid carries the block hash.
3. zkboost compares the slot of the header of that child with the slot of the payload.
4. zkboost signs the envelope under `DOMAIN_EXECUTION_PROOF` with the fork version at the slot of the block. It takes that fork version from the chain spec, as a validator client does.
5. zkboost sends the envelope with `POST /eth/v1/beacon/execution_proofs`. The body is the SSZ encoding of a list of at most 4 envelopes and has the header `Content-Type: application/octet-stream`.

zkboost submits no proof in these cases.

- Two children carry the payload in their bid, which shows a proposer equivocation. A Gloas payload bid commits to the slot and the parent, therefore one child only is valid.
- The slot of the child header differs from the slot of the payload. The execution header does not commit to the slot, therefore zkboost compares the two sources.

The table gives the EIP-8025 proof type of every zkboost proof type.

| `proof_type` | zkboost proof type |
| ------------ | ------------------ |
| `1`          | `ethrex-openvm`    |
| `2`          | `ethrex-sp1`       |
| `3`          | `ethrex-zisk`      |
| `4`          | `reth-openvm`      |
| `5`          | `reth-sp1`         |
| `6`          | `reth-zisk`        |
| `7`          | `zesu-zisk`        |

## Mock Beacon Node

`mock-beacon-node` mocks a beacon node with the EIP-8025 behavior. It stands in for the beacon node in the examples.

- It serves the Engine API to a CL and forwards every request to zkboost with the JWT of the CL unchanged. zkboost therefore receives the Engine API traffic of a real CL.
- It receives the proofs at `POST /eth/v1/beacon/execution_proofs`. It looks up the beacon block of every envelope at the CL and verifies the validator signature as the beacon node does.
- It verifies each proof with `ere-verifier`. The check covers the request root, a successful validation, the `DEPOSIT_CHAIN_ID` of the CL, and the Amsterdam schema id.
- The mock proof bytes `MOCK` pass as valid.
- It forwards every other beacon API request to the CL. zkboost therefore uses the mock beacon node as its `cl_beacon_endpoint`.

| Flag                       | Description                                 |
| -------------------------- | ------------------------------------------- |
| `--cl-endpoint <URL>`      | Beacon API endpoint of the CL whose Engine API requests the mock forwards |
| `--zkboost-endpoint <URL>` | Engine API endpoint of zkboost              |
| `--proof-types <a,b>`      | Proof types expected for every payload      |
| `--port <u16>`             | Port serving the beacon API (default: 3001) |
| `--engine-port <u16>`      | Port serving the Engine API to the CL (default: 8551) |

## Observability

zkboost exposes Prometheus-compatible metrics at `/metrics` for monitoring with Prometheus and Grafana.

### Docker Compose with Grafana

The Docker Compose setup includes pre-configured Prometheus and Grafana with a zkboost dashboard. Start the Kurtosis testnet as in [docker/example/observability](docker/example/observability) first.

```bash
docker compose -f docker/example/observability/docker-compose.yml up -d
```

| Service    | URL                   | Credentials   |
| ---------- | --------------------- | ------------- |
| zkboost    | http://localhost:3000 | -             |
| Prometheus | http://localhost:9090 | -             |
| Grafana    | http://localhost:3002 | admin / admin |

The zkboost dashboard is auto-provisioned and available at Grafana > Dashboards > zkboost.

### Available Metrics

| Metric                                   | Type      | Description                                     |
| ---------------------------------------- | --------- | ----------------------------------------------- |
| `zkboost_http_requests_total`            | Counter   | Total HTTP requests by endpoint, method, status |
| `zkboost_http_request_duration_seconds`  | Histogram | Request latency by endpoint                     |
| `zkboost_http_requests_in_flight`        | Gauge     | Currently processing requests                   |
| `zkboost_witness_fetch_total`            | Counter   | Witness fetch round trips by status             |
| `zkboost_witness_fetch_duration_seconds` | Histogram | Witness fetch round trip time                   |
| `zkboost_witness_bytes`                  | Histogram | Witness sizes of valid payloads                 |
| `zkboost_queue_wait_duration_seconds`    | Histogram | Time a proof request waits for a worker         |
| `zkboost_prove_total`                    | Counter   | Prove operations by proof type and status       |
| `zkboost_prove_duration_seconds`         | Histogram | Proof generation time                           |
| `zkboost_prove_proof_bytes`              | Histogram | Generated proof sizes                           |
| `zkboost_programs_loaded`                | Gauge     | Number of loaded zkVMs                          |
| `zkboost_build_info`                     | Gauge     | Build version info                              |

## Supported Backends

zkboost integrates backends through Ere. Any backend that Ere supports can be added.

## Contributing

Contributions are welcome!

## License

Dual‑licensed under **Apache‑2.0** and **MIT**. Choose either license at your discretion.
