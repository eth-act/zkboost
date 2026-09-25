
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

# zkVM backends. Without a [[zkvm]] entry, zkboost forwards every request unchanged and proves nothing.

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

# Mock zkVMs (in-process). The mock sleeps for the simulated proving time and returns the fixture
# proof of the proof type, a valid proof of another block. A zesu-zisk mock is rejected, because
# its guest has no fixture proof.

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

The CL uses zkboost as its Engine API endpoint at `POST /`.

- zkboost forwards every request to `el_engine_endpoint` with the JWT of the CL. The EL validates the JWT.
- zkboost sends `engine_newPayloadV5` to the EL as `engine_newPayloadWithWitnessV5` and removes the `witness` from the answer.
- For a `VALID` payload, every configured zkVM proves the stateless execution in the background.
- zkboost takes the protocol fork of a payload from its slot. The max blob count of the blob schedule selects the BPO fork, Amsterdam for the BPO2 max of 21.
- A payload imported again after a reorg gets its cached proof.

zkboost also serves these endpoints.

| Method | Endpoint            | Purpose                             |
| ------ | ------------------- | ----------------------------------- |
| `GET`  | `/health`           | Health check                        |
| `GET`  | `/metrics`          | Prometheus metrics                  |
| `GET`  | `/dashboard`        | Built-in dashboard UI (when enabled) |
| `GET`  | `/dashboard/state`  | Dashboard state JSON snapshot       |
| `GET`  | `/dashboard/events` | Dashboard SSE event stream          |

## Proof Submission

zkboost signs every proof as the configured validator and posts it to `cl_beacon_endpoint` as an EIP-8025 `SignedExecutionProofEnvelope`. It reads these routes of the beacon API.

| Route                                                | Reads                                           | How often                  |
| ---------------------------------------------------- | ----------------------------------------------- | -------------------------- |
| `GET /eth/v1/config/spec`                            | The chain spec                                  | At startup                 |
| `GET /eth/v1/beacon/genesis`                         | The genesis validators root                     | At startup                 |
| `GET /eth/v1/beacon/states/head/validators/{pubkey}` | The index of the signing validator              | At startup                 |
| `GET /eth/v1/events?topics=execution_payload`        | The beacon block root of every imported payload | Streamed                   |
| `GET /eth/v1/beacon/headers?parent_root=`            | The children of the parent beacon block root    | Per proof without an event |
| `GET /eth/v2/beacon/blocks/{root}`                   | The payload bid of a listed child               | Per listed child           |

zkboost forwards payloads without proving until the beacon node answers the startup routes. zkboost submits no proof if two child blocks carry the payload, or if the child block has another slot.

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

`mock-beacon-node` stands in for an EIP-8025 beacon node in the examples.

- It forwards the Engine API of a CL to zkboost, and every other beacon API request to the CL.
- It verifies the signature and the proof of every submitted envelope. The public values must be the ones of the fixture proofs, so a proof of a live payload fails.

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
