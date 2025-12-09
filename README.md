# zkBoost

A zkVM proving server that supports multiple zkVM backends through a unified interface.

## Overview

zkBoost provides a server (`poost`) that compiles and serves zkVM programs, exposing endpoints for execution, proving, and verification.

## Project Structure

```
zkboost/
├── bins/
│   ├── poost-cli/       # CLI to run the poost server
│   └── client-cli/      # CLI client for interacting with the server
├── crates/
│   ├── poost-core/      # Core types, config, and program handling
│   ├── poost-server/    # Axum-based HTTP server
│   └── poost-client/    # Client library
└── guest-programs/      # Example zkVM guest programs
```

## Supported zkVMs

- SP1
- Risc0
- Jolt
- Pico
- OpenVM
- Miden
- Nexus
- Airbender
- Ziren
- Zisk

## Configuration

Create a `poost-config.yaml` file:

```yaml
server_url: "127.0.0.1:3000"

program_instances:
  - name: "fibonacci"
    zkvm_name: "sp1"
    program_path: "./guest-programs/fibonacci"
```

| Field | Description |
|-------|-------------|
| `server_url` | Address and port for the server |
| `program_instances` | List of programs to compile and serve |
| `name` | Human-readable program name |
| `zkvm_name` | Target zkVM backend |
| `program_path` | Path to the guest program source |

## Running the Server

```bash
# With default config (poost-config.yaml)
cargo run -p poost-cli

# With custom config path
cargo run -p poost-cli -- --config /path/to/config.yaml
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | GET | Health check |
| `/info` | GET | Server and program info |
| `/execute` | POST | Execute a program |
| `/prove` | POST | Generate a proof |
| `/verify` | POST | Verify a proof |