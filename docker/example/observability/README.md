# Observability Stack with zkboost

This example runs zkboost with three mock zkVM backends, `mock-zkattestor`, Prometheus, Tempo, and Grafana against a local Kurtosis testnet.

The testnet has three participants.

- The first two participants produce the blocks.
- The third participant has the geth behind zkboost and no validators. Its lighthouse is stopped, so only zkboost feeds the geth.

The proof flow has three steps.

- The attestor follows the lighthouse of the first participant and sends every Gloas payload to zkboost as `engine_newPayloadV5`.
- zkboost forwards the payload to the third geth, proves it with the mocks, and exports its spans to Tempo.
- zkboost submits the signed proofs to the attestor, which verifies them.

The `reth-sp1` mock has `mock_failure = true`. It shows the failure path and makes the attestor wait its proof timeout for every block.

## Start

1. Run `kurtosis run --enclave local-testnet github.com/ethpandaops/ethereum-package --args-file docker/example/observability/network_params.yaml`.
2. Run `docker compose -f docker/example/observability/docker-compose.yml up -d`.
3. Run `kurtosis service stop local-testnet cl-3-lighthouse-geth`.

geth answers a known payload with `VALID` and no witness, so the third geth must receive every payload through zkboost. Start the attestor before the stop, so that the third geth misses no payload.

| Service    | URL                             | Credentials   |
| ---------- | ------------------------------- | ------------- |
| zkboost    | http://localhost:3000/dashboard | -             |
| Prometheus | http://localhost:9090           | -             |
| Grafana    | http://localhost:3002           | admin / admin |

The zkboost dashboard of Grafana is provisioned under Dashboards.

## Stop

1. Run `docker compose -f docker/example/observability/docker-compose.yml down`.
2. Run `kurtosis enclave rm -f local-testnet`.
