
<p align="center">
  <img src="assets/logo.png" width="auto" height="40" alt="zkboost logo" />
</p>

<p align="center"><b>Simple HTTP API for any Ere-compliant zkVM.</b></p>

zkboost is an API wrapper on top of [Ere](https://github.com/eth-act/ere) allowing you to execute, verify and create zkVM proofs by calling a HTTP endpoint.

## Table of Contents

- [Table of Contents](#table-of-contents)
- [Features](#features)
- [Quick Start](#quick-start)
- [Manual Build](#manual-build)
  - [Prerequisites](#prerequisites)
- [API](#api)
- [Observability](#observability)
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Features

* **REST API** for execution, proof generation, and verification.
* **Pluggable Backend Support**: Leverages `Ere` for backend integration.
* **Prometheus Metrics**: Built-in observability via `/metrics` endpoint.

## Quick Start

The easiest way to start is by running the `GITHUB_TOKEN=<github-token> RUST_LOG=info cargo test --package zkboost-server --test stateless_validator -- --zkvm sp1 --resource cpu`.

> The `GITHUB_TOKEN` is needed to download compiled artifact from repo [`eth-act/zkevm-benchmark-workload`](https://github.com/eth-act/zkevm-benchmark-workload).

## Manual Build

### Prerequisites

* **Rust** ≥ 1.88

```bash
# 1. Clone
git clone https://github.com/eth-act/zkboost.git && cd zkboost

# 2. Run
cargo run --release
```

## API

The following endpoints are available:

| Endpoint   | Method | Purpose                                  |
| ---------- | ------ | ---------------------------------------- |
| `/info`    | `GET`  | Get server and system information        |
| `/execute` | `POST` | Run program and get execution metrics    |
| `/prove`   | `POST` | Generate proof for a program with inputs |
| `/verify`  | `POST` | Verify a previously generated proof      |
| `/metrics` | `GET`  | Prometheus metrics endpoint              |

## Observability

zkboost exposes Prometheus-compatible metrics at `/metrics` for monitoring with Prometheus and Grafana.

### Docker Compose with Grafana

The Docker Compose setup includes pre-configured Prometheus and Grafana with a zkboost dashboard:

```bash
cd docker && docker-compose up -d
```

| Service    | URL                     | Credentials     |
| ---------- | ----------------------- | --------------- |
| zkboost    | http://localhost:3000   | -               |
| Prometheus | http://localhost:9090   | -               |
| Grafana    | http://localhost:3001   | admin / admin   |

The zkboost dashboard is auto-provisioned and available at Grafana > Dashboards > zkboost.

### Available Metrics

| Metric | Type | Description |
| ------ | ---- | ----------- |
| `zkboost_http_requests_total` | Counter | Total HTTP requests by endpoint, method, status |
| `zkboost_http_request_duration_seconds` | Histogram | Request latency by endpoint |
| `zkboost_http_requests_in_flight` | Gauge | Currently processing requests |
| `zkboost_prove_total` | Counter | Prove operations by program and status |
| `zkboost_prove_duration_seconds` | Histogram | Proof generation time |
| `zkboost_prove_proof_bytes` | Histogram | Generated proof sizes |
| `zkboost_execute_total` | Counter | Execute operations by program and status |
| `zkboost_execute_duration_seconds` | Histogram | Execution time |
| `zkboost_execute_cycles_total` | Histogram | zkVM cycle counts |
| `zkboost_verify_total` | Counter | Verify operations by program and result |
| `zkboost_verify_duration_seconds` | Histogram | Verification time |
| `zkboost_programs_loaded` | Gauge | Number of loaded zkVM programs |
| `zkboost_build_info` | Gauge | Build version info |

## Supported Backends

zkboost uses `Ere` for backend integration. Not all backends will be integrated, however since the API for Ere is uniform, it is easy to add backends already supported by Ere.

## Contributing

Contributions are welcome!

## License

Dual‑licensed under **Apache‑2.0** and **MIT**. Choose either license at your discretion.
