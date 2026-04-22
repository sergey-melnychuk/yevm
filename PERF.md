# yaevmi — Performance Analysis

This document analyses the codebase for runtime performance. All findings assume
tracing/logging infrastructure is disabled at the call site (e.g. no `mpsc::Sender`
attached, no external consumer of events). The goal is to identify work that is paid
on every opcode execution even when no one is observing it.

---

## 1. `Event::Step` allocation on every opcode — independent, immediate fix

**Location:** [`yaevmi-core/src/evm.rs:398–444`](yaevmi-core/src/evm.rs#L398-L444)

On every opcode `step()` unconditionally constructs a `Step` struct regardless of whether
anyone is consuming it:

1. `name.to_string()` → heap `String` (lines 399–403)
2. `self.data(pc)` → heap `Vec<u8>` for PUSH immediates (lines 404–409)
3. `Step { debug: vec![], ... }` → inner `Vec<String>` (line 419)
4. `step.debug.push(format!("cost={cost}"))` → format + string (line 440)
5. `state.emit(Event::Step(step))` → pushes `Trace` into `cache.events` (line 444)

`Event::Step` is the one event variant `revert_to()` ignores (`_ => None` at
[`cache.rs:442`](yaevmi-core/src/cache.rs#L442)), so it is **not** needed for reverts.
This part can be gated immediately without any other changes.

Also: the SSTORE handler ([`store.rs:139–146`](yaevmi-core/src/ops/store.rs#L139-L146))
checks `if let Some(step) = evm.step.as_mut()` — which is always `Some` — and
unconditionally calls five `format!()` macros per SSTORE. Same fix.

**Fix:** Add `fn is_tracing(&self) -> bool` to `State`. Gate the Step construction block
and the SSTORE debug block behind it. This is self-contained and does not require §2.

---

## 2. Many `emit()` variants are trace-only and can be gated immediately

**Location:** [`yaevmi-core/src/cache.rs:371–386`](yaevmi-core/src/cache.rs#L371-L386),
[`cache.rs:425–444`](yaevmi-core/src/cache.rs#L425-L444)

Not all events are load-bearing for reverts. `revert_to()` uses a `filter_map` that
returns `Some(Revert::...)` for exactly 10 variants and `_ => None` for everything else.
The load-bearing variants are:

`Put::Store`, `Put::Nonce`, `Put::Value`, `Put::Temp`, `Put::Code`,
`WarmAcc`, `WarmKey`, `Create`, `Delete`, `Log`

Every other variant — `Get`, `Move`, `Hash`, `Code` (the non-Put one), `Call`, `Return`,
`Revert`, `Fee`, `Halt`, `Blob`, `Step` — is trace-only. These are currently emitted
unconditionally and pushed into `events` even when no trace consumer exists.

`Event::Get` in particular fires on every SLOAD read, every balance check, every nonce
read. On a storage-heavy trace this is one of the most frequent events and produces no
revert effect.

**Immediate fix (no architecture change required):** Add an `is_tracing()` early-return
inside `emit()` that skips the `events.push()` for all trace-only variants. The 10
load-bearing variants still go through unconditionally.

**Full fix (architectural prerequisite for eliminating the rest):** `revert_to()` scans
`events` linearly and the Vec grows monotonically (reverted entries are marked, not
removed). Replace with a dedicated `revert_log: Vec<Revert>` — the `enum Revert` at
[`cache.rs:46–56`](yaevmi-core/src/cache.rs#L46-L56) already has every needed variant.
`checkpoint()` returns `revert_log.len()`; `revert_to()` truncates and replays in
reverse. Once done, `events` is purely observational, the O(n) scan in `revert_to()` is
gone, and memory is bounded by call depth rather than total transaction history.

---

## 3. `lift()` round-trips through big-endian bytes on every arithmetic op — medium-high impact

**Location:** [`yaevmi-base/src/int.rs:34–44`](yaevmi-base/src/int.rs#L34-L44)

`Int` is a type alias for `Hex<32>` — a newtype over `[u8; 32]`. Arithmetic uses the
`lift()` adapter:

```rust
pub fn lift<const N: usize>(f: impl Fn([U256; N]) -> U256) -> impl Fn([Int; N]) -> Int {
    move |xs| {
        // [u8; 32] → U256  (U256::from_be_slice — 32-byte parse)
        // run f
        // U256 → [u8; 32]  (to_be_bytes — 32-byte serialize)
    }
}
```

Every `ADD`, `SUB`, `MUL`, `LT`, `EQ`, `ISZERO`, `AND`, `OR`, `SHL`, `SHR` … pays two
32-byte big-endian conversions per operand plus one for the result. A typical DeFi
contract executes millions of these.

**Fix:** Change the EVM stack type from `Vec<Int>` to `Vec<U256>` (`ruint::Uint<256,4>`).
`U256` is already `Copy` and 32 bytes, so the stack layout is identical. Arithmetic
operates directly on `U256`. Convert to/from `[u8; 32]` only at ABI boundaries (CALLDATALOAD,
MLOAD/MSTORE, SLOAD/SSTORE, PUSH immediates, LOG topics). This eliminates the per-opcode
round-trip entirely.

---

## 4. `pending_stack_push: Vec<Int>` for 0–1 element — medium impact

**Location:** [`yaevmi-core/src/evm.rs:158, 277`](yaevmi-core/src/evm.rs#L158)

Almost every opcode pops 1–3 values and pushes exactly 0 or 1. The pending push buffer
is a `Vec<Int>` that oscillates between empty and length-1. This is the worst case for
`Vec`: each push checks capacity, may reallocate, and `drain(..)` in `apply()` zeroes
the length pointer.

**Fix:** Replace `Vec<Int>` with `arrayvec::ArrayVec<Int, 2>` (or a manual
`Option<[Int; 2]>`). For the rare DUP/SWAP patterns that push 2 values, 2 slots suffice.
This keeps the struct on the stack with zero heap involvement.

Similarly `pending_acc_warmup: Vec<Acc>` and `pending_key_warmup: Vec<(Acc, Int)>` are
almost always 0 or 1 element — the same fix applies.

---

## 5. Nested `Vec<u8>` in `pending_mem_stores` — medium impact

**Location:** [`yaevmi-core/src/evm.rs:163`](yaevmi-core/src/evm.rs#L163),
[`evm.rs:357–361`](yaevmi-core/src/evm.rs#L357-L361)

```rust
pub(crate) pending_mem_stores: Vec<(usize, usize, Vec<u8>)>,
```

`mem_put()` calls `source.to_vec()` to copy the source slice into a heap allocation that
survives until `apply()` commits it moments later. For MSTORE, MSTORE8, CALLDATACOPY,
RETURNDATACOPY, and MCOPY this happens once per opcode.

**Fix:** `mem_put()` can write directly to `self.memory` (after `mem_expand`) instead of
staging through a `Vec`. The deferred-commit pattern exists to roll back failed opcodes,
but the only consumer of `pending_mem_stores` is `apply()`, called unconditionally on
success. Failed opcodes call `reset()` which discards the pending stores — so staging is
necessary only if `mem_expand` can fail (OutOfGas). A simpler approach: snapshot the
memory length before writing, and on gas failure restore it. This eliminates the inner
`Vec<u8>` allocation on every memory-writing opcode.

---

## 6. Default SipHash on all hot collections — medium impact

**Location:** [`yaevmi-core/src/cache.rs:60–66`](yaevmi-core/src/cache.rs#L60-L66)

```rust
accounts:   HashMap<Acc, AccountEntry>,          // key = [u8; 20]
transient:  HashMap<(Acc, Int), Int>,            // key = 52 bytes
warm_accs:  HashSet<Acc>,                        // key = [u8; 20]
warm_keys:  HashSet<(Acc, Int)>,                 // key = 52 bytes
```

All use Rust's default `SipHash-1-3` which is DoS-resistant but ~3× slower than
non-cryptographic alternatives on small fixed-size keys. SLOAD/SSTORE and every warmth
check hashes 20–52 bytes through SipHash on the critical path.

**Fix:** Add `ahash` (already used transitively in many Ethereum crates) or
`rustc-hash`/`fxhash` as a dependency and replace with
`HashMap<K, V, AHasher>` / `HashSet<K, AHasher>`. Both are safe for EVM keys (no
adversarial key control in the cache). Expected improvement: 2–3× on SLOAD/SSTORE-heavy
workloads.

---

## 7. `created()` / `destroyed()` allocate Vec on every call — low-medium impact

**Location:** [`yaevmi-core/src/cache.rs:331–337`](yaevmi-core/src/cache.rs#L331-L337)

```rust
fn created(&self) -> Vec<Acc> {
    self.created.iter().cloned().collect()   // allocates
}
fn destroyed(&self) -> Vec<Acc> {
    self.destroyed.iter().cloned().collect() // allocates
}
```

These are called during transaction finalization. The allocation is minor but avoidable
by changing the `State` trait to return an iterator or take a callback.

---

## 8. `data(pc)` always allocates for PUSH immediate — low impact (when tracing disabled)

**Location:** [`yaevmi-core/src/evm.rs:373–385`](yaevmi-core/src/evm.rs#L373-L385)

`data()` returns a `Vec<u8>` on every step even for non-PUSH opcodes (returns empty
`vec![0; 0]` via `vec![0; len]` with `len=0`). With the tracing fix from §1, this call
disappears entirely when tracing is off — so this is only a residual concern if something
else calls `data()` outside tracing.

---

## 9. Double memory expansion in `mem_put` → `mem_store` — low impact

**Location:** [`yaevmi-core/src/evm.rs:307–361`](yaevmi-core/src/evm.rs#L307-L361)

`mem_put()` calls `mem_expand()` to charge gas and grow the vector, then `apply()` calls
`mem_store()` which calls `mem_expand()` again (line 349). The second call is a no-op
because the memory is already the right size, but it still evaluates the bounds math and
branch. If the pending-store staging is removed (§5), this duplication disappears.

---

## 10. Sequential RPC fetching — medium impact for live use

**Location:** [`yaevmi-core/src/rpc.rs:90`](yaevmi-core/src/rpc.rs#L90) (existing TODO)

Each `Fetch` yield interrupts execution, fires one RPC call, resumes. Multi-account
transactions (e.g. ERC-20 transfer touching sender, receiver, token contract) issue
fetches serially. Concurrent pre-fetching or speculative fetching of likely-needed
accounts would amortize round-trip latency.

---

## 11. PUSH decodes its immediate twice — medium impact

**Location:** [`yaevmi-core/src/ops/stack.rs:8`](yaevmi-core/src/ops/stack.rs#L8),
[`yaevmi-core/src/evm.rs:404`](yaevmi-core/src/evm.rs#L404)

`step()` calls `self.data(pc)` for tracing (allocates `Vec<u8>`), and then the `push`
handler calls `evm.data(evm.pc)` again to decode the actual immediate value. PUSH1 — one
of the most common opcodes in any compiled contract — pays two heap allocations per
execution. The tracing call disappears with the §1 fix. The handler call can be
eliminated by reading the immediate as a direct slice into `self.code` and building `Int`
in-place without allocating.

---

## 12. Pending mechanism is unnecessary for infallible ops — medium impact

**Location:** [`yaevmi-core/src/evm.rs:213–232`](yaevmi-core/src/evm.rs#L213-L232)

The `peek()` → compute → `pending_stack_push.push()` → `apply()` pattern exists to allow
rollback if an opcode fails mid-execution. For pure arithmetic ops (ADD, MUL, LT, EQ,
ISZERO, AND, OR, XOR, NOT, SHL, SHR, …) the only possible failure is `gas_charge()`,
which is always called **first** before any stack reads. Once gas succeeds these ops
cannot fail, so the pending machinery is pure overhead.

SWAP already demonstrates this — [`stack.rs:39–41`](yaevmi-core/src/ops/stack.rs#L39-L41)
mutates `evm.stack` directly after the underflow check with no pending buffer. The same
direct-mutation pattern can be applied to all arithmetic ops: check gas → pop directly →
compute → push directly. This eliminates the Vec bounce in `pending_stack_push` and the
associated loops in `apply()` for approximately 60% of opcode executions.

---

## Priority order for implementation

| # | Change | Estimated gain | Complexity |
|---|--------|---------------|------------|
| 1 | Gate Step construction + SSTORE debug behind `is_tracing()` | Very high | Low |
| 2 | Separate revert log from trace events | High | Medium |
| 3 | Stack type `Vec<U256>` instead of `Vec<Int>` | High | Medium |
| 4 | match-based dispatch + generic `S: State` (eliminate vtable) | High (storage-heavy) | Medium |
| 5 | Direct stack mutation for infallible arithmetic ops | Medium | Medium |
| 6 | `ahash` / `fxhash` for cache collections | Medium | Low |
| 7 | Precomputed JUMPDEST bitmap (also fixes spec compliance) | Medium | Low |
| 8 | Fix double `data()` call in PUSH handler | Medium | Low |
| 9 | Inline `mem_put` writes, eliminate pending `Vec<u8>` | Medium | Medium |
| 10 | `ArrayVec` for remaining pending warmup buffers | Low-Medium | Low |
| 11 | Iterator-based `created()`/`destroyed()` | Low | Low |
| 12 | Concurrent RPC fetching | Medium (live) | High |

Items 1, 6, 7, and 8 are small, safe, and immediately profitable. Item 2 is a
prerequisite for fully eliminating tracing overhead. Items 3 and 4 are the largest
structural changes and have the highest ceiling for storage-heavy workloads; they
can be done independently. Item 5 builds naturally after item 3.
