# Mock Attestor with zkboost

This example runs zkboost with two mock zkVM backends against a local Kurtosis testnet, with `mock-zkattestor` in place of the beacon node.

The testnet has three participants.

- The first two participants produce the blocks.
- The third participant has the geth behind zkboost and no validators. Its lighthouse is stopped, so only zkboost feeds the geth.

The proof flow has three steps.

- The attestor follows the lighthouse of the first participant and sends every Gloas payload to zkboost as `engine_newPayloadV5`.
- zkboost forwards the payload to the third geth and proves it with the mocks.
- zkboost submits the signed proofs to the attestor, which verifies them.

## Start

1. Run `kurtosis run --enclave local-testnet github.com/ethpandaops/ethereum-package --args-file docker/example/mock-zkattestor/network_params.yaml`.
2. Run `docker compose -f docker/example/mock-zkattestor/docker-compose.yml up -d`.
3. Run `kurtosis service stop local-testnet cl-3-lighthouse-geth`.

geth answers a known payload with `VALID` and no witness, so the third geth must receive every payload through zkboost. Start the attestor before the stop, so that the third geth misses no payload.

The dashboard of zkboost is served at http://localhost:3000/dashboard.

## Stop

1. Run `docker compose -f docker/example/mock-zkattestor/docker-compose.yml down`.
2. Run `kurtosis enclave rm -f local-testnet`.
