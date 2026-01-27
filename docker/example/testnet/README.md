# Local Testnet with EWS

This example needs to be ran with the local testnet https://github.com/eth-act/lighthouse/blob/optional-proofs/scripts/local_testnet/network_params_mixed_proof_gen_verify.yaml, which runs 3 normal node and 3 optional-proof node.

The `zkboost-server` and `execution-witness-sentry` (EWS) is configured to have 2 proof type `ethrex-zisk` and `reth-zisk`.

## Build image with GPU acceleration

To make sure EWS can keep up with the testnet, we can build the `ere-server-zisk` image with GPU acceleration if it is available.

```
git clone --depth 1 --branch v0.1.0 https://github.com/eth-act/ere
cd ere
COMPUTE_CAP=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d '.')
bash .github/scripts/build-image.sh --zkvm zisk --tag local --server --cuda --cuda-arch "sm_$COMPUTE_CAP"
```

This builds the image `ere-server-zisk:local-cuda`

## Start local testnet

In `zkboost` repo:

```
cd ./docker/example/testnet/scripts
./start_local_testnet.sh -n network_params_mixed_proof_gen_verify.yaml
```

## Start EWS

Configure the GPU resoure in `./docker/example/testnet/docker-compose.yml`, by default it assumes 8 GPUs are available, and distributes 4 to each prover.

In `zkboost` repo:

```
docker compose -f ./docker/example/testnet/docker-compose.yml build
docker compose -f ./docker/example/testnet/docker-compose.yml up -d
```
