# Amsterdam in yevm

Reference: execution-specs `tests@v21.0.3` (2026-11-20, no consensus changes for Frontier
through Amsterdam since v21.0.2): the Python spec under `src/ethereum/forks/amsterdam` and its
fixtures. yevm keeps Osaka (and the BPO forks) and adds Amsterdam behind a fork switch, so a
caller such as Helios can run both: mainnet is on Osaka, Sepolia on Amsterdam.

## EIPs

EVM and transaction level (what `eth_call` needs):

| EIP | What | Status |
|---|---|---|
| 7843 | SLOTNUM opcode (slot number from the block header) | [x] |
| 8024 | DUPN, SWAPN, EXCHANGE | [x] |
| 7954 | Larger maximum contract and initcode size | [x] |
| 7976 | Higher calldata floor cost | [x] |
| 7981 | Access lists count toward the floor | [x] |
| 2780 | Resource-based intrinsic transaction gas | [x] |
| 8038 | State-access gas repricing | [x] |
| 8246 | SELFDESTRUCT no longer burns balance | [x] |
| 7708 | ETH transfers emit a log | [x] |
| 8037 | State creation gas: a second gas dimension (state gas reservoir) | [x] |

Block level (the reth node, not `eth_call`):

| EIP | What | Status |
|---|---|---|
| 7778 | Block gas accounting without refunds | [x] |
| 7928 | Block-level access lists, built by the executor | [x] |
| 7997 | Deterministic CREATE2 factory at the fork | [x] (no executor change: the factory is in the chain's state) |
| 8282 | Builder execution requests | [x] |

## Spec notes (execution-specs `src/ethereum/forks/amsterdam`, v21.0.3)

Constants that change: COLD_ACCOUNT_ACCESS 2600 -> 3000; TX_BASE 21000 -> 12000; TX_VALUE_COST
6000 (new); CREATE_ACCESS = ACCOUNT_WRITE 9000 + COLD 3000 (replaces CREATE 32000 and TX_CREATE);
CALL_VALUE = 9000 + 2300 stipend (the caller now pays the stipend); STORAGE_WRITE 10000 exec;
REFUND_STORAGE_CLEAR = (10000 + 2100) * 4800 / 5000; TX_DATA_TOKEN_FLOOR 10 -> 16; access list
2900 per address, 2000 per key; TX_MAX_TOTAL_GAS_LIMIT 2^32 - 1 (TX_MAX_GAS_LIMIT 2^24 still caps
execution gas); blob schedule target 14, max 21, fraction 11,684,671.

State gas (EIP-8037), COST_PER_STATE_BYTE 1530: NEW_ACCOUNT 120 bytes (183,600), STORAGE_SET 64
bytes (97,920), AUTH_BASE 23 bytes, code deposit 1530 per byte (plus 6 per word exec for hashing).

Intrinsic (`transactions.calculate_intrinsic_cost`): TX_BASE + recipient (create: CREATE_ACCESS
+ 2/word initcode; call to another account: COLD 3000 + 6000 if value; self-transfer: 0) + data
(zero byte 4, other 16) + access list (2900/2000 + floor tokens 80/128 x 16, the token part added to
both intrinsic and floor) + EXECUTION_PER_AUTH_BASE_COST (101 x 16 + 3000 + 3000 + 200 = 7816) per
authorization. Floor = TX_BASE + recipient + len(data) x 64 + access-list tokens x 16.

Gas meter: gas_left, state_gas_left (reservoir), baseline, spilled, committed_spill, refunds.
- allocate: evm_gas = gas - intrinsic; execution = min(2^24 - intrinsic, evm_gas); reservoir =
  the rest.
- charge_state(x): from the reservoir, else spill the remainder into gas_left (spilled += it),
  else out of gas.
- child frame: gets the call's execution gas and the parent's whole reservoir (drained), with
  baseline = that reservoir.
- revert/halt of a frame: gas_left += spilled, reservoir = baseline, refunds = 0 (halt then
  forfeits gas_left).
- incorporate child: parent += child's gas_left, reservoir, spilled, refunds; on success repay
  spills from the reservoir. A NEW_ACCOUNT the parent charged for a failed child is credited back
  (to spilled first, then the reservoir).
- top frame (`create_evm`): set_delegation (state gas) then commit; create: NEW_ACCOUNT if the
  address was empty in the pre-state; call: NEW_ACCOUNT if value goes to a non-alive account.
  A failure here restores the reservoir to entry and forfeits gas.
- settle: used = gas - gas_left - reservoir_left; refund = min(used / 5, refunds); paid =
  max(used - refund, floor). Block gas = max(sum execution, sum state) (EIP-7778/8037).

Opcodes: CALL with value to a non-alive account charges NEW_ACCOUNT state gas (no 25000), before
the 63/64 split. CREATE/CREATE2: CREATE_ACCESS + initcode, NEW_ACCOUNT state if the address is not
alive (credited back on failure). SELFDESTRUCT to a dead beneficiary with balance: 9000 exec +
NEW_ACCOUNT state. Value moves emit a log from the system address (EIP-7708).

## Steps

1. Fork switch: a spec value on the block head, Osaka by default; the Osaka suite must not
   change.
2. Harness: run the v21.0.3 state tests for Osaka (regression) and Amsterdam.
3. The EVM-level EIPs, simplest first, each checked against the Amsterdam state tests.
4. 8037 last among them: it touches every charge that creates state.
5. Block level in yevm-reth, checked with the Amsterdam blockchain tests. Done.
6. Helios: yevm for Osaka and Amsterdam blocks, then drop revm as Helios's Ethereum engine.

## Log

- 2026-11-23: `Spec` on `Head` (Osaka default) and `Head::slot_number`; `yevm-test/src/eest.rs`
  runs the v21.0.3 state tests through yevm alone, with the pre-state served as a `Chain`.
  Osaka: 14,280 of 14,664 pass; the other 384 are invalid transactions yevm accepts, since
  it leaves transaction validation to its host. Amsterdam baseline: 1,575 of 16,907.
- 2026-11-23: the EVM-level EIPs. `yevm-core/src/gas.rs` holds the per-fork costs; `Gas` has
  the EIP-8037 reservoir (charge, spill, credit, restore, commit, repay), with state charges
  pending in `Evm` like the others. The executor allocates execution gas and the reservoir,
  applies Amsterdam delegations after the fees under a checkpoint, charges the top frame's new
  account and delegated-code access, hands the reservoir to children and merges it back, and
  settles with the reservoir. Fixed on the way: a reverted code change restored empty code
  instead of the previous code (only reachable once delegations can be rolled back).
  Amsterdam: 16,415 of 16,907 pass, every executed case; the other 492 are invalid
  transactions (host validation). Osaka unchanged at 14,280.
- 2026-11-23: block level in yevm-reth: the head's fork and slot number, separate execution
  and state block gas, evm2's Amsterdam gas caps, EIP-8282 builder requests, balance-keeping
  destroys, the state gas reservoir for system calls, and the block access list. The list is
  built with evm2's `Bal` from each transaction's changes and the reads yevm-core records
  where the specification reads state (`State::access`, after the guarding gas checks, kept
  across reverts); authorities are read one by one as they are charged. Blocks whose list
  exceeds the gas limit's item budget are rejected. yevm-reth also builds on macOS again.
  Blockchain tests (v21.0.3, through reth's executor and consensus checks, access list hash
  included): 43,880 of 43,880 pass, Osaka and Amsterdam.
