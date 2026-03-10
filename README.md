
<p align="center">
  <img src="assets/logo.png" width="270" alt="zkboost logo" />
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
  - [Docker Compose with Grafana](#docker-compose-with-grafana)
  - [Available Metrics](#available-metrics)
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Features

* **REST API** for execution, proof generation, and verification.
* **Pluggable Backend Support**: Leverages `Ere` for backend integration.
* **Prometheus Metrics**: Built-in observability via `/metrics` endpoint.

## Quick Start

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

| Method | Endpoint                                                       | Purpose                                                       |
| ------ | -------------------------------------------------------------- | ------------------------------------------------------------- |
| `POST` | `/v1/execution_proof_requests?proof_types=`                    | Submit SSZ-encoded `NewPayloadRequest` to request for a proof |
| `GET`  | `/v1/execution_proof_requests?new_payload_request_root=`       | SSE stream of proof result                                    |
| `GET`  | `/v1/execution_proofs/{new_payload_request_root}/{proof_type}` | Fetch a completed proof                                       |
| `POST` | `/v1/execution_proof_verifications`                            | Verify a proof                                                |
| `GET`  | `/health`                                                      | Health check                                                  |
| `GET`  | `/metrics`                                                     | Prometheus metrics                                            |

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
| `zkboost_programs_loaded`               | Gauge     | Number of loaded zkVM programs                  |
| `zkboost_build_info`                    | Gauge     | Build version info                              |

## Supported Backends

zkboost uses `Ere` for backend integration. Not all backends will be integrated, however since the API for Ere is uniform, it is easy to add backends already supported by Ere.

## Contributing

Contributions are welcome!

## License

Dual‑licensed under **Apache‑2.0** and **MIT**. Choose either license at your discretion.
