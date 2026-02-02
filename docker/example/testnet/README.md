# Local Testnet with EWS

This example shows how to run a small local testnet with 3 normal nodes, and 3 optional-proofs nodes, with zkboost generating EL execution proofs, and EWS (Execution Witness Sentry) publish the proofs (configured to have 2 proof types `ethrex-zisk` and `reth-zisk`).

```mermaid
sequenceDiagram
    participant CL as CL
    participant EL as EL
    participant CLOptionalProofs as CL <br> (optional-proofs)
    participant EWS as EWS
    participant zkboost as zkboost <br> server
    participant ere as Ere <br> server(s)
    CL->>EWS: New head <br> (SSE)
    EL->>EWS: Fetch block + witness
    EWS->>zkboost: Request proof
    zkboost->>EWS: Response proof_gen_id
    zkboost->>ere: Request proof
    ere->>zkboost: Response proof
    zkboost->>EWS: Send proof + proof_gen_id
    EWS->>CLOptionalProofs: Submit proof
```

## Installation

1. Install [Docker](https://docs.docker.com/get-docker/). Verify that Docker has been successfully installed by running `sudo docker run hello-world`. 

1. Install [Kurtosis](https://docs.kurtosis.com/install/). Verify that Kurtosis has been successfully installed by running `kurtosis version` which should display the version.

1. Install [`yq`](https://github.com/mikefarah/yq). If you are on Ubuntu, you can install `yq` by running `snap install yq`.

## Build image with GPU acceleration

To make sure EWS can keep up with the testnet, we can build the `ere-server-zisk` image with GPU acceleration if it is available.

```
git clone --depth 1 --branch v0.1.0 https://github.com/eth-act/ere
cd ere
COMPUTE_CAP=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d '.')
bash .github/scripts/build-image.sh --zkvm zisk --tag local --base --server --cuda --cuda-arch "sm_$COMPUTE_CAP"
```

This builds the image `ere-server-zisk:local-cuda`

## Start local testnet

In `zkboost` repo:

```
./docker/example/testnet/start_local_testnet.sh -n ./docker/example/testnet/network_params_mixed_proof_gen_verify.yaml
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

## Stop zkboost and EWS

In `zkboost` repo:

```
docker compose -f ./docker/example/testnet/docker-compose.yml down
```
