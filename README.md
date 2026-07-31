# YEVM

**(Yield-aware | Yet another) EVM implementation** — in Rust.

**[Live Demo](https://sergey-melnychuk.github.io/yevm)**

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

| Crate        | Description                                                       |
| ------------ | ------------------------------------------------------------------|
| `yevm-base`  | Primitive types: `Acc` (address), `Int` (uint256), `Head`, `Tx`   |
| `yevm-misc`  | Utilities and helpers                                             |
| `yevm-core`  | EVM engine: opcode dispatch, stack/memory, `State`/`Chain` traits |
| `yevm-lens`  | Side-effect decoder: ERC-20/721 transfers, approvals, proxy swaps |
| `yevm-gate`  | Local RPC proxy: simulate and approve transactions before sending |
| `yevm-wasm`  | WebAssembly bindings for in-browser transaction simulation        |
| `yevm`       | Full integration: ties all crates together                        |
| `yevm-test`  | Test harness and fixtures (not published)                         |

## Links

1. [YellowPaper](https://ethereum.github.io/yellowpaper/paper.pdf)

2. [Ethereum EIPs](https://eips.ethereum.org)

3. [EVM.codes](https://www.evm.codes/)

## Licensing

YEVM is licensed under the PolyForm Noncommercial License 1.0.0. 
It is free for educational use, research, and non-commercial tooling.

### Commercial Use
If you wish to use YEVM for commercial purposes, embed it into revenue-generating 
infrastructure, or operate it for commercial advantage, the PolyForm license does 
not apply. Please contact me directly to discuss commercial licensing 
and revenue-sharing terms.
