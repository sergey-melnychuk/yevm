# Amsterdam in yevm

Reference: execution-specs `tests@v21.0.3` (2026-11-20, no consensus changes for Frontier
through Amsterdam since v21.0.2): the Python spec under `src/ethereum/forks/amsterdam` and its
fixtures. yevm keeps Osaka (and the BPO forks) and adds Amsterdam behind a fork switch, so a
caller such as Helios can run both: mainnet is on Osaka, Sepolia on Amsterdam.

## EIPs

EVM and transaction level (what `eth_call` needs):

| EIP | What | Status |
|---|---|---|
| 7843 | SLOTNUM opcode (slot number from the block header) | [ ] |
| 8024 | DUPN, SWAPN, EXCHANGE | [ ] |
| 7954 | Larger maximum contract and initcode size | [ ] |
| 7976 | Higher calldata floor cost | [ ] |
| 7981 | Access lists count toward the floor | [ ] |
| 2780 | Resource-based intrinsic transaction gas | [ ] |
| 8038 | State-access gas repricing | [ ] |
| 8246 | SELFDESTRUCT no longer burns balance | [ ] |
| 7708 | ETH transfers emit a log | [ ] |
| 8037 | State creation gas: a second gas dimension (state gas reservoir) | [ ] |

Block level (the reth node, not `eth_call`):

| EIP | What | Status |
|---|---|---|
| 7778 | Block gas accounting without refunds | [ ] |
| 7928 | Block-level access lists, built by the executor | [ ] |
| 7997 | Deterministic CREATE2 factory at the fork | [ ] |
| 8282 | Builder execution requests | [ ] |

## Steps

1. Fork switch: a spec value on the block head, Osaka by default; the Osaka suite must not
   change.
2. Harness: run the v21.0.3 state tests for Osaka (regression) and Amsterdam.
3. The EVM-level EIPs, simplest first, each checked against the Amsterdam state tests.
4. 8037 last among them: it touches every charge that creates state.
5. Block level in yevm-reth, checked with the Amsterdam blockchain tests.
6. Helios: yevm for Osaka and Amsterdam blocks, then drop revm as Helios's Ethereum engine.

## Log

- 2026-11-23: `Spec` on `Head` (Osaka default) and `Head::slot_number`; `yevm-test/src/eest.rs`
  runs the v21.0.3 state tests through yevm alone, with the pre-state served as a `Chain`.
  Osaka: 14,280 of 14,664 pass; the other 384 are invalid transactions yevm accepts, since
  it leaves transaction validation to its host. Amsterdam baseline: 1,575 of 16,907.
