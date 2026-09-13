
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
- [Mock Attestor](#mock-attestor)
- [Observability](#observability)
  - [Docker Compose with Grafana](#docker-compose-with-grafana)
  - [Available Metrics](#available-metrics)
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Quick Start

See [docker/example/testnet](docker/example/testnet) for a Docker Compose setup that runs zkboost with real Ere backends on a local testnet. See [docker/example/mock-zkattestor](docker/example/mock-zkattestor) for mock backends without a GPU. There `mock-zkattestor` stands in for the beacon node.

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

# External proving cluster (ZisK proof types only)
[[zkvm]]
kind = "cluster"
proof_type = "reth-zisk"
endpoint = "http://zisk-cluster:50051"
elf_url = "https://example.com/stateless-validator-reth-zisk.elf"
# elf_path = "/path/to/program.elf"   # mutually exclusive with elf_url

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

## Engine API

zkboost serves the Engine API as JSON-RPC at `POST /`. The CL uses zkboost as its Engine API endpoint in place of the EL.

- Every request is forwarded byte-for-byte to `el_engine_endpoint`. The EL response is returned to the CL.
- The `Authorization` JWT header of the CL is passed through unchanged. The EL validates the JWT. zkboost does not validate it.
- zkboost reads the chain id once with `eth_chainId` on `el_engine_endpoint`, authenticated with the JWT of the CL.
- `engine_newPayloadV5` is intercepted. zkboost sends it to the EL as `engine_newPayloadWithWitnessV5`, an EL extension that returns the execution witness in the payload status. geth, nethermind, nimbus, and ethrex implement the method. reth serves the witness only on the SSZ Engine API route `POST /engine/v1/payloads/witness` of execution-apis PR 885, which zkboost does not use.
- zkboost removes the `witness` field from the payload status before it answers the CL.
- When the payload status is `VALID`, zkboost builds the stateless input from the payload, the witness, and the chain id. It then dispatches proving to every configured zkVM backend in the background.
- Every payload is proven under the Amsterdam rules. `engine_newPayloadV1` to `engine_newPayloadV4` are forwarded without proving.
- A repeated `engine_newPayload` for a payload that is already proven is submitted again with the cached proof, so a payload imported again after a reorg is not proven twice. zkboost keeps the proofs of the last 64 payloads per proof type, two epochs.

The following endpoints are also available:

| Method | Endpoint            | Purpose                             |
| ------ | ------------------- | ----------------------------------- |
| `GET`  | `/health`           | Health check                        |
| `GET`  | `/metrics`          | Prometheus metrics                  |
| `GET`  | `/dashboard`        | Built-in dashboard UI (when enabled) |
| `GET`  | `/dashboard/state`  | Dashboard state JSON snapshot       |
| `GET`  | `/dashboard/events` | Dashboard SSE event stream          |

## Proof Submission

zkboost delivers every generated proof to the beacon node at `cl_beacon_endpoint` as an EIP-8025 `SignedExecutionProofEnvelope`, with `POST /eth/v1/beacon/execution_proofs`. The body is the SSZ encoding of a list of at most 4 `SignedExecutionProofEnvelope`, sent with `Content-Type: application/octet-stream`.

zkboost reads the chain spec, the genesis validators root, and the index of its validator once, with `GET /eth/v1/config/spec`, `GET /eth/v1/beacon/genesis`, and `GET /eth/v1/beacon/states/head/validators/{pubkey}`. For every proof it lists the children of the parent beacon block root with `GET /eth/v1/beacon/headers?parent_root=` and submits an envelope under every child whose payload bid carries the block hash, as a block imported again after a reorg is. It signs each envelope under `DOMAIN_EXECUTION_PROOF` with the fork version at the slot of the block, computed from the chain spec as a validator client does. The execution proof types are provisional. lighthouse assigns 1 to 3 to the reth guests, and zkboost continues the sequence for the ethrex guests.

| `proof_type` | zkboost proof type |
| ------------ | ------------------ |
| `1`          | `reth-openvm`      |
| `2`          | `reth-sp1`         |
| `3`          | `reth-zisk`        |
| `4`          | `ethrex-openvm`    |
| `5`          | `ethrex-sp1`       |
| `6`          | `ethrex-zisk`      |

A beacon node accepts proofs of Gloas blocks only, since EIP-8025 is built on Gloas. A failed submission is logged. zkboost does not retry it.

## Mock Attestor

`mock-zkattestor` stands in for a CL that attests to execution proofs. It follows the block events of a CL and sends every Gloas payload to zkboost as `engine_newPayloadV5`. It receives the proofs at `POST /eth/v1/beacon/execution_proofs`, verifies the validator signature as the beacon node does, and verifies each proof with `ere-verifier` against the request root, a successful validation, the `DEPOSIT_CHAIN_ID` of the CL, and the Amsterdam schema id. The mock proof bytes `MOCK` pass as valid. Every other beacon API request is forwarded to the CL, so zkboost uses the attestor as its `cl_beacon_endpoint`.

The EL behind zkboost must not know a payload before the attestor sends it. geth answers a known payload with `VALID` and no witness. In a Kurtosis testnet, the attestor therefore follows the CL of one participant while zkboost forwards to the EL of another participant whose CL is stopped, so that EL receives every payload through zkboost.

| Flag                                       | Description                                                                                                                  |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| `--cl-endpoint <URL>`                      | Beacon API endpoint of the CL to follow                                                                                      |
| `--zkboost-endpoint <URL>`                 | Engine API endpoint of zkboost                                                                                               |
| `--proof-types <a,b>`                      | Proof types expected for every payload                                                                                       |
| `--port <u16>`                             | Port serving the beacon API (default: 3001)                                                                                  |
| `--program-vk <PROOF_TYPE>=<PATH or URL>`  | Program verifying key per proof type (repeatable). Proofs of a type without a verifying key are only accepted as `MOCK`.     |

Verifying keys are published next to the ELFs in the [ere-guests v0.17.0 release](https://github.com/eth-act/ere-guests/releases/tag/v0.17.0), which matches the ere version, e.g. `https://github.com/eth-act/ere-guests/releases/download/v0.17.0/stateless-validator-reth-zisk-v1.1.0-alpha.vk`.

## Observability

zkboost exposes Prometheus-compatible metrics at `/metrics` for monitoring with Prometheus and Grafana.

### Docker Compose with Grafana

The Docker Compose setup includes pre-configured Prometheus and Grafana with a zkboost dashboard. Start the Kurtosis testnet as in [docker/example/mock-zkattestor](docker/example/mock-zkattestor) first.

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
