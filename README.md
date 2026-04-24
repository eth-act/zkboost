
<p align="center">
  <img src="assets/logo.png" width="270" alt="zkboost logo" />
</p>

<p align="center"><b>EIP 8025 Proof Node Implementation</b></p>

zkboost is an EIP 8025 Proof Node implementation for CL to request execution proof generation and verification.

## Table of Contents

- [Table of Contents](#table-of-contents)
- [Quick Start](#quick-start)
- [Manual Build](#manual-build)
  - [Prerequisites](#prerequisites)
- [Configuration](#configuration)
- [API](#api)
- [Observability](#observability)
  - [Docker Compose with Grafana](#docker-compose-with-grafana)
  - [Available Metrics](#available-metrics)
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Quick Start

See [docker/example/testnet](docker/example/testnet) for a Docker Compose setup that runs zkboost with real Ere backends on a local testnet.

## Manual Build

### Prerequisites

* **Rust** ≥ 1.93

```bash
# 1. Clone
git clone https://github.com/eth-act/zkboost.git && cd zkboost

# 2. Build
cargo build --release

# 3. Run
./target/release/zkboost --config <config-path>
```

## Configuration

zkboost is configured via a TOML file passed with `--config <path>`. Below is an annotated example showing all options:

```toml
# HTTP server port (default: 3000)
port = 3000

# Ethereum execution layer JSON-RPC endpoint (required)
el_endpoint = "http://localhost:8545"

# Optional local chain config JSON file
# chain_config_path = "path/to/chain_config.json"

# Timeout for witness fetching in seconds (default: 12)
# witness_timeout_secs = 12

# LRU cache size for completed proofs (default: 128)
# proof_cache_size = 128

# LRU cache size for execution witnesses (default: 128)
# witness_cache_size = 128

# External Ere server (calls a remote ere-server via HTTP)
[[zkvm]]
kind = "ere"
proof_type = "ethrex-zisk"

# Timeout for proof generation in seconds (default: 12)
# proof_timeout_secs = 12

# Endpoint of the Ere server
endpoint = "http://ere-server:3000"

# Mock zkVMs (in-process, for testing without Docker/GPU)

# Fixed proving time (default)
[[zkvm]]
kind = "mock"
proof_type = "reth-sp1"
mock_proving_time = { kind = "constant", ms = 6000 }

# Random proving time uniformly sampled from [min_ms, max_ms]
[[zkvm]]
kind = "mock"
proof_type = "reth-zisk"
mock_proving_time = { kind = "random", min_ms = 2000, max_ms = 8000 }

# Proving time proportional to block gas (ms_per_mgas * gas_used / 1000_000)
[[zkvm]]
kind = "mock"
proof_type = "ethrex-zisk"
mock_proving_time = { kind = "linear", ms_per_mgas = 300 }

# Simulated failure (always returns a proving error)
[[zkvm]]
kind = "mock"
proof_type = "reth-risc0"
mock_failure = true
```

Available proof types:

| Index | Name           | EL       | zkVM      |
| ----- | -------------- | -------- | --------- |
| `0`   | `ethrex-risc0` | `ethrex` | RISC Zero |
| `1`   | `ethrex-sp1`   | `ethrex` | SP1       |
| `2`   | `ethrex-zisk`  | `ethrex` | ZisK      |
| `3`   | `reth-openvm`  | `reth`   | OpenVM    |
| `4`   | `reth-risc0`   | `reth`   | RISC Zero |
| `5`   | `reth-sp1`     | `reth`   | SP1       |
| `6`   | `reth-zisk`    | `reth`   | ZisK      |

## API

The following endpoints are available:

| Method | Endpoint                                                       | Purpose                                                       |
| ------ | -------------------------------------------------------------- | ------------------------------------------------------------- |
| `POST` | `/v1/execution_proof_requests?proof_types=`                    | Submit SSZ-encoded `NewPayloadRequest` to request for a proof |
| `GET`  | `/v1/execution_proof_requests?new_payload_request_root=`       | SSE stream of proof result                                    |
| `GET`  | `/v1/execution_proofs/{new_payload_request_root}/{proof_type}` | Fetch a completed proof                                       |
| `POST` | `/v1/execution_proof_verifications`                            | Verify a proof                                                |
| `GET`  | `/health`                                                      | Health check                                                  |
| `GET`  | `/metrics`                                                     | Prometheus metrics                                            |

See [openapi.json](openapi.json) for the full API specification ([rendered](https://petstore.swagger.io/?url=https://raw.githubusercontent.com/eth-act/zkboost/master/openapi.json)).

## Observability

zkboost exposes Prometheus-compatible metrics at `/metrics` for monitoring with Prometheus and Grafana.

### Docker Compose with Grafana

The Docker Compose setup includes pre-configured Prometheus and Grafana with a zkboost dashboard:

```bash
cd docker/example/observability && docker-compose up -d
```

| Service    | URL                   | Credentials   |
| ---------- | --------------------- | ------------- |
| zkboost    | http://localhost:3000 | -             |
| Prometheus | http://localhost:9090 | -             |
| Grafana    | http://localhost:3002 | admin / admin |

The zkboost dashboard is auto-provisioned and available at Grafana > Dashboards > zkboost.

### Available Metrics

| Metric                                  | Type      | Description                                     |
| --------------------------------------- | --------- | ----------------------------------------------- |
| `zkboost_http_requests_total`           | Counter   | Total HTTP requests by endpoint, method, status |
| `zkboost_http_request_duration_seconds` | Histogram | Request latency by endpoint                     |
| `zkboost_http_requests_in_flight`       | Gauge     | Currently processing requests                   |
| `zkboost_prove_total`                   | Counter   | Prove operations by program and status          |
| `zkboost_prove_duration_seconds`        | Histogram | Proof generation time                           |
| `zkboost_prove_proof_bytes`             | Histogram | Generated proof sizes                           |
| `zkboost_verify_total`                  | Counter   | Verify operations by program and result         |
| `zkboost_verify_duration_seconds`       | Histogram | Verification time                               |
| `zkboost_programs_loaded`               | Gauge     | Number of loaded zkVMs                          |
| `zkboost_build_info`                    | Gauge     | Build version info                              |

## Supported Backends

zkboost uses `Ere` for backend integration. Not all backends will be integrated, however since the API for Ere is uniform, it is easy to add backends already supported by Ere.

## Contributing

Contributions are welcome!

## License

Dual‑licensed under **Apache‑2.0** and **MIT**. Choose either license at your discretion.
