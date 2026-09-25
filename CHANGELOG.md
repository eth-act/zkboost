# Changelog

## [0.11.0](https://github.com/eth-act/zkboost/compare/v0.10.2...v0.11.0) (2026-09-25)


### Features

* refactor ([#83](https://github.com/eth-act/zkboost/issues/83)) ([9d20c2a](https://github.com/eth-act/zkboost/commit/9d20c2a42bca61891b6b26065d64fc4f7a156268))

## [0.10.2](https://github.com/eth-act/zkboost/compare/v0.10.1...v0.10.2) (2026-09-25)


### Continuous Integration

* tag zkboost image with full sha as well ([#81](https://github.com/eth-act/zkboost/issues/81)) ([0e212b3](https://github.com/eth-act/zkboost/commit/0e212b3a3a283a057e0ba45f122a42fc84784e54))

## [0.10.1](https://github.com/eth-act/zkboost/compare/v0.10.0...v0.10.1) (2026-09-24)


### Features

* add zkvm_versions to proof-engine.json ([#78](https://github.com/eth-act/zkboost/issues/78)) ([eff3a46](https://github.com/eth-act/zkboost/commit/eff3a46f28bb425e1ad30fe341ccc1947afcebe9))
* allow no zkvm configured ([#79](https://github.com/eth-act/zkboost/issues/79)) ([ab828f0](https://github.com/eth-act/zkboost/commit/ab828f0d1230c678e0f011c24f5fbb23bad6318e))
* make mock-guests with standard guest io ([#80](https://github.com/eth-act/zkboost/issues/80)) ([63e508f](https://github.com/eth-act/zkboost/commit/63e508f42d85cb1393b7666772936bf1b2ba94ec))
* update docker example to use ethpandaops images ([#76](https://github.com/eth-act/zkboost/issues/76)) ([aa7a2b4](https://github.com/eth-act/zkboost/commit/aa7a2b4e34e0865a00a3507869b025af1b3ee58b))

## [0.10.0](https://github.com/eth-act/zkboost/compare/v0.9.0...v0.10.0) (2026-09-21)


### Features

* rework zkboost as middleware between CL and EL ([#75](https://github.com/eth-act/zkboost/issues/75)) ([3bd9f4b](https://github.com/eth-act/zkboost/commit/3bd9f4bcb965b19078782fc3c8be0dafb64e939f))
* **server,types:** stage-timing observability — queue-wait metric, self-describing completions, identified spans ([#71](https://github.com/eth-act/zkboost/issues/71)) ([ce4390d](https://github.com/eth-act/zkboost/commit/ce4390d4e5ce0a6e64323c00229a67e31d7ade40))
* update ere and ere-guests to v0.14.0 ([#73](https://github.com/eth-act/zkboost/issues/73)) ([4d02f8f](https://github.com/eth-act/zkboost/commit/4d02f8f1152b0e508e6e4bd19fb3f67fea205a47))

## [0.9.0](https://github.com/eth-act/zkboost/compare/v0.8.0...v0.9.0) (2026-07-08)


### Features

* upgrade ere-guests to v0.13.0 ([#68](https://github.com/eth-act/zkboost/issues/68)) ([033636d](https://github.com/eth-act/zkboost/commit/033636d3d62def699f5813252a396eaa1d3c4ef7))

## [0.8.0](https://github.com/eth-act/zkboost/compare/v0.7.0...v0.8.0) (2026-06-09)


### Features

* bump ere and add cluster kind with zisk cluster support ([#66](https://github.com/eth-act/zkboost/issues/66)) ([732fcbb](https://github.com/eth-act/zkboost/commit/732fcbb8d5aa853af9b6581ff1e4e2a75e38bc66))
* reject proof requests early for verifier-only instances ([#64](https://github.com/eth-act/zkboost/issues/64)) ([5e4d30b](https://github.com/eth-act/zkboost/commit/5e4d30b866123e80169104fe9fe818ad129bfb25))
* add proof types endpoint ([#65](https://github.com/eth-act/zkboost/issues/65)) ([9be2085](https://github.com/eth-act/zkboost/commit/9be2085d87e130674d0b2f7abb0679541e20c426))

## [0.7.0](https://github.com/eth-act/zkboost/compare/v0.6.0...v0.7.0) (2026-05-13)


### Features

* **server:** in-process verifier-only zkVM backend (kind: verifier) ([#61](https://github.com/eth-act/zkboost/issues/61)) ([d8aec0b](https://github.com/eth-act/zkboost/commit/d8aec0b1aca9240cc6bb77377c641bdc90b78c60))
* support all zkVMs' in-process verifier ([#63](https://github.com/eth-act/zkboost/issues/63)) ([4344cf8](https://github.com/eth-act/zkboost/commit/4344cf89ea77f14294c02236e4ceeb03241708e4))

## [0.6.0](https://github.com/eth-act/zkboost/compare/v0.5.0...v0.6.0) (2026-04-23)


### Features

* add `kurtosis.yml` ([#57](https://github.com/eth-act/zkboost/issues/57)) ([f08ca16](https://github.com/eth-act/zkboost/commit/f08ca165636cca48d4d7792ee34426106a74b890))
* update `ere` to `v0.8.1` ([#59](https://github.com/eth-act/zkboost/issues/59)) ([c6f0e03](https://github.com/eth-act/zkboost/commit/c6f0e03ed48bec254dfbb03e65095fffe15e7989))

## [0.5.0](https://github.com/eth-act/zkboost/compare/v0.4.2...v0.5.0) (2026-04-10)


### Features

* update config to move proof timeout into per zkvm config ([#54](https://github.com/eth-act/zkboost/issues/54)) ([0013038](https://github.com/eth-act/zkboost/commit/00130383fa9611b5be327da3f3918b39ff737d88))
