# yevm — Performance Analysis

Benchmark: block 24929490, 373 mainnet transactions, offline prefetched state, N=1000.

---

## Done

### §1 — Gate `Event::Step` construction behind `is_tracing()`

**Result:** 0.36 s → 0.096 s (3.75×)

`Step` struct (heap `String`, `Vec<u8>`, `Vec<String>`) was unconditionally allocated on
every opcode. Gated behind `state.is_tracing()` which checks `cache.sender.is_some()`.
`Event::Step` has no revert semantics so it is safe to skip entirely.

### §6 — `ahash` for cache `HashMap`/`HashSet`

**Result:** 0.096 s → 0.094 s (~2%)

Replaced `std::collections::{HashMap, HashSet}` with `ahash` equivalents in `cache.rs`.

### §A — Eliminate vtable: generic dispatch over `S: State`

**Result:** combined with §B+§C+§D: 0.094 s → 0.077 s (~18%)

Replaced the `OPS: [(&str, Handler); 256]` function-pointer table with a `dispatch<S: State>`
match expression. State-calling handlers (`sload`, `sstore`, `tload`, `tstore`, `balance`,
`extcodesize`, `call`, `log`, etc.) were made generic. The compiler can now inline `Cache`
methods through these call sites and eliminate all vtable dispatch for the hot path.

Handlers in `basic.rs` are not generic — they ignore state entirely (`_: &mut dyn State`)
so there is no vtable overhead there regardless.

### §B — PUSH immediate: direct slice, no allocation

**Result:** combined with §A+§C+§D: 0.094 s → 0.077 s (~18%)

Replaced `evm.data(evm.pc)` (allocates `Vec<u8>`, then copies into `Int`) with a direct
read into a stack-allocated `[u8; 32]` buffer. Eliminates one heap allocation per PUSH
opcode. The tracing path in `step()` still calls `data()` but only when `is_tracing()`.

### §C — `mem_put`: write directly, drop `pending_mem_stores`

**Result:** combined with §A+§B+§D: 0.094 s → 0.077 s (~18%)

Removed `pending_mem_stores: Vec<(usize, usize, Vec<u8>)>`. `mem_put()` now writes directly
to `self.memory` after `mem_expand`. Safe because `mem_put` is always the last fallible
operation in any handler — nothing can fail after the write.

### §D — `ArrayVec`/`Option` for pending warmup buffers

**Result:** combined with §A+§B+§C: 0.094 s → 0.077 s (~18%)

Replaced `pending_acc_warmup: Vec<Acc>` with `[Acc; 2]` + `pending_acc_count: usize`, and
`pending_key_warmup: Vec<(Acc, Int)>` with `Option<(Acc, Int)>`. Max occupancy is 2 for
acc (CALL + EIP-7702 delegate) and 1 for key (only SSTORE). All warmup state is now
stack-allocated, eliminating heap churn on every opcode that touches access lists.

---

## Future

Profile at 0.077 s: `Evm::step` 30%, `ec_mul` 21%, `ecrecover` 17%, `ec_pairing` 10%.
Roughly half of block time is elliptic-curve precompile work.

### §E — `secp256k1` (libsecp256k1) for `ecrecover` on native targets

**Estimate:** ~17% → ~6% of block time

`pre/ecrecover.rs` uses pure-Rust `k256`, which recovers roughly 2-3× slower than
libsecp256k1. revm uses the `secp256k1` crate on native targets for this reason.

All 17% is precompile 0x01 invoked from contracts (permit-style signature checks) — `bench`
takes the sender from prefetched RPC data, so none of it is tx sender recovery.

Must be feature-gated with `k256` as the default, so `yevm-wasm` keeps building without a
C toolchain.

### §F — `pairing_batch` for `ec_pairing`

**Estimate:** ~10% → ~6% of block time

`pre/bn128.rs` computes `acc = acc * pairing(g1, g2)` per pair, running a full Miller loop
*and* a final exponentiation each time. Final exponentiation is ~40-50% of a pairing, so
collecting the pairs and issuing one `bn::pairing_batch(&[(G1, G2)])` (verified present in
`substrate-bn` 0.6) is close to 2× on the multi-pair inputs Groth16 verifiers produce.
Contained to one function, no dependency or feature changes.

### Not worth doing

`ec_mul` (21%) already runs on `substrate-bn`, the backend revm defaults to — no library
swap closes it, and arkworks is a port rather than a drop-in. BN128 gas is underpriced
relative to plain opcodes for every client, so a mainnet-block gas/sec figure partly
measures block composition rather than interpreter speed.

---

## Priority

| Item | Change | Status |
|------|--------|--------|
| §1 | Gate `Event::Step` behind `is_tracing()` | **done** (3.75×) |
| §6 | `ahash` for cache collections | **done** (~2%) |
| §A | Generic `S: State` dispatch to eliminate vtable | **done** (~18% combined) |
| §B | PUSH immediate: direct slice, no alloc | **done** (~18% combined) |
| §C | `mem_put`: write direct, drop pending `Vec<u8>` | **done** (~18% combined) |
| §D | `Option`/fixed array for pending warmup buffers | **done** (~18% combined) |
| §E | `secp256k1` for `ecrecover` on native (feature-gated) | todo (~11% est.) |
| §F | `pairing_batch` for `ec_pairing` | todo (~4% est.) |
