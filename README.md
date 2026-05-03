# YAEVMI

**(Yield-Aware | Yet Another) EVM Implementation** — in Rust.

**[Live Demo](https://sergey-melnychuk.github.io/yaevmi)**

## Goals

- **Async-first** — execution yields on every external state access (account, storage, code)
- **WebAssembly-native** — runs in-browser or in edge environments without modification
- **Observability** — full tracing, clean error types, inspectable state at every step
- **Performance** — execution performance comparable to production-grade EVM implementations
- **Correctness** — 99+% on GeneralStateTests at Cancun (mainnet is now Osaka/Fusaka)
- **Mainnet-oriented** — focus on supporting only latest Ethereum mainnet hard-fork
- **Infra-agnostic** — needs only RPC URL, can use service provider or own hosted node

Intended for: 
- education: learning Ethereum VM internals and edge cases
- devtools: debugging tx execution and gas consumption
- security: simulate tx and expose balance & state changes
- testing: embeddable Rust crate for testing smart-contracts

## Crates

| Crate         | Description                                                       |
| ------------- | ----------------------------------------------------------------- |
| `yaevmi-base` | Primitive types: `Acc` (address), `Int` (uint256), `Head`, `Tx`   |
| `yaevmi-core` | EVM engine: opcode dispatch, stack/memory, `State`/`Chain` traits |
| `yaevmi-misc` | Utilities and helpers                                             |
| `yaevmi-wasm` | WebAssembly bindings                                              |
| `yaevmi-test` | Test harness and fixtures                                         |
| `yaevmi-full` | Full integration: ties all crates together                        |

## Links

1. [YellowPaper](https://ethereum.github.io/yellowpaper/paper.pdf)

2. [Ethereum EIPs](https://eips.ethereum.org)

3. [EVM.codes](https://www.evm.codes/)
