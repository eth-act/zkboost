# Local Testnet with zkboost

This example runs a local Ethereum testnet (via Kurtosis) alongside the zkboost with 2 Ere GPU provers, configured for `ethrex-zisk` and `reth-zisk` proof types.

```mermaid
sequenceDiagram
    participant CL as CL <br> (Kurtosis)
    participant zkboost as zkboost <br> server
    participant EL as EL <br> (Kurtosis)
    participant Ere as Ere <br> server(s)
    CL->>zkboost: POST /v1/execution_proof_requests <br> (SSZ NewPayloadRequest)
    zkboost->>CL:
    CL->>zkboost: GET /v1/execution_proof_requests?new_payload_request_root=0x... <br> (SSE stream)
    zkboost->>EL: Fetch ExecutionWitness
    EL->>zkboost:
    zkboost->>Ere: Request proof
    Ere->>zkboost:
    zkboost->>CL: proof_complete or proof_failure SSE
    CL->>zkboost: GET /v1/execution_proofs/{new_payload_request_root}/{type}
    zkboost->>CL:
```

## Installation

1. Install [Docker](https://docs.docker.com/get-docker/). Verify that Docker has been successfully installed by running `sudo docker run hello-world`.

1. Install [Kurtosis](https://docs.kurtosis.com/install/). Verify that Kurtosis has been successfully installed by running `kurtosis version` which should display the version.

1. Install [`yq`](https://github.com/mikefarah/yq). If you are on Ubuntu, you can install `yq` by running `snap install yq`.

## (Optional) Build image locally with GPU acceleration

The pre-built ZisK prover image (`ghcr.io/eth-act/ere/ere-server-zisk:0.6.0-cuda`) supports Blackwell GPUs only (ZisK only supports single architecture codegen). If you have a Blackwell GPU, e.g. RTX 50 series or RTX PRO 6000, skip this section.

Build the image with the compute capability of local GPU:

```bash
git clone --depth 1 --branch v0.6.0 https://github.com/eth-act/ere
cd ere
CUDA_ARCH=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d '.')
echo "Building for CUDA architecture: $CUDA_ARCH"
bash .github/scripts/build-image.sh \
    --registry ghcr.io/eth-act/ere \
    --zkvm zisk \
    --tag 0.6.0-cuda \
    --base \
    --server \
    --cuda \
    --cuda-archs "$CUDA_ARCH"
```

This produces `ghcr.io/eth-act/ere/ere-server-zisk:0.6.0-cuda`, which is referenced by the `./docker/example/testnet/docker-compose.yml`.

## Start local testnet

In `zkboost` repo:

```
./docker/example/testnet/start_local_testnet.sh
```

## Start zkboost and EWS

Configure the GPU resoure in `./docker/example/testnet/docker-compose.yml`, by default it assumes 8 GPUs are available, and distributes 4 to each prover.

In `zkboost` repo:

```
docker compose -f ./docker/example/testnet/docker-compose.yml build
docker compose -f ./docker/example/testnet/docker-compose.yml up -d
```

## Stop local testnet

In `zkboost` repo:

```
./docker/example/testnet/stop_local_testnet.sh
```

## Stop zkboost

In `zkboost` repo:

```
docker compose -f ./docker/example/testnet/docker-compose.yml down
```
