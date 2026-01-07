# zkboost with ZisK cluster

## Step 1: Build ZisK distributed prover image


```bash
CUDA_ARCH=sm_120 ./docker/zisk-cluster/build-image.sh
```

This builds image `zisk-distributed:v0.15.0-gpu`

## Step 2: Build zkboost

```bash
docker compose -f ./docker/zisk-cluster/docker-compose.yml build
```

This spins up 1 coordinator and 1 worker by default, adding more `zisk-worker-x` in `./docker/zisk-cluster/docker-compose.yml` if multiple GPUs are available.

## Step 3: Start zkboost server ZisK cluster

```bash
docker compose -f ./docker/zisk-cluster/docker-compose.yml up -d
```

## Step 4: Run empty block proving

```bash
cargo test --package zkboost-server --test stateless_validator --release -- \
    --zkboost-server-url http://127.0.0.1:3001 \
    --el reth \
    --zkvm zisk
```

## Step 5: Run any block proving

```bash
# TODO: Add a cli to load block fixture, or listen to latest mainnet and send request to `zkboost-server`
```
