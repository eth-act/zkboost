
<p align="center">
  <img src="assets/logo.png" width="240" alt="zkboost logo" />
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
- [Supported Backends](#supported-backends)
- [Contributing](#contributing)
- [License](#license)

## Features

* **REST API** for execution, proof generation, and verification.
* **Pluggable Backend Support**: Leverages `Ere` for backend integration.

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

## Supported Backends

zkboost uses `Ere` for backend integration. Not all backends will be integrated, however since the API for Ere is uniform, it is easy to add backends already supported by Ere.

## Contributing

Contributions are welcome!

## License

Dual‑licensed under **Apache‑2.0** and **MIT**. Choose either license at your discretion.
