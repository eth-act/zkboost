# Local Testnet with EWS

This example needs to be ran with the local testnet https://github.com/eth-act/lighthouse/blob/optional-proofs/scripts/local_testnet/network_params_mixed_proof_gen_verify.yaml, which runs 3 normal node and 3 optional-proof node.

The `zkboost-server` and `execution-witness-sentry` (EWS) is configured to have 2 proof type `ethrex-zisk` and `reth-zisk`.

## Build image with GPU acceleration

To make sure EWS can keep up with the testnet, we can build the `ere-server-zisk` image with GPU acceleration if it is available.

```
git clone --depth 1 --revision a9fb04dea82e658b900bf09fdf8a816fa4eec59b https://github.com/eth-act/ere
cd ere
ZKVM=zisk CUDA=1 .github/scripts/build-image
```

## Start local testnet

In `lighthouse` repo:

```
cd ./scripts/local_testnet
./start_local_testnet.sh -n network_params_mixed_proof_gen_verify.yaml
```

## Start EWS

Configure the GPU resoure in `/docker/example/testnet/docker-compose.yml`, by default it assumes 8 GPUs are available, and distributes 4 to each prover.

In `zkboost` repo:

```
docker compose -f ./docker/example/testnet/docker-compose.yml build
docker compose -f ./docker/example/testnet/docker-compose.yml up -d
```
