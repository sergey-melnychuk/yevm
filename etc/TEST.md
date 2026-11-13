# Tests

## EEST blockchain tests, Osaka (yevm as reth's EVM)

The conformance suite for the node: every Osaka case from the Ethereum execution-spec-tests
(EEST), run through reth's block executor with `YevmEvmConfig`, the same configuration the
`node` binary uses. Each case imports its blocks into an in-memory world state. A block is
accepted only if it decodes, passes reth's consensus checks before and after execution (gas used,
receipts root, logs bloom, requests hash), and its state root, computed independently with
`alloy-trie`, matches the header. Blocks the fixture marks invalid must be rejected. At the end,
the head must be the fixture's last block and the world state must equal its post-state exactly.

```
etc/eest.sh     # EEST v5.4.0 fixtures_develop -> tmp/eest/v5.4.0-develop (~420 MB download, ~8 GB)
cargo test -p yevm-reth --profile eest --test eest -- --ignored --nocapture
```

`EEST_EVM=evm2` runs the same cases on reth's own EVM. `EEST_FILTER`, `EEST_THREADS` and
`EEST_TRACE` are described in `yevm-reth/tests/eest.rs`. The first build compiles all of
reth, about 15 minutes with `-j 12`. Don't run it next to a live node on a small machine.

2026-11-13, reth 46731e9, 6 threads:

```
=== EEST blockchain tests, yevm on Osaka,OsakaToBPO1AtTime15k,BPO1ToBPO2AtTime15k,BPO2ToBPO3AtTime15k,BPO3ToBPO4AtTime15k ===
passed: 21567
failed: 14 (0 panics, 14 known: tests/eest-known-failures.list)
 TOTAL: 21581
  time: 84s
```

- **The 14 known failures** are EIP-7610 creation collisions: a creation targets an address that
  has storage but no code and nonce 0, which the spec says must fail. evm2 fails the same 14
  (`EEST_EVM=evm2`: same pass count, 130 s). Neither EVM can see the storage through the
  database interface reth gives it. Reaching such an address on mainnet takes a hash collision.
  - yevm and evm2 fail them with the same errors: `tmp/eest/report-yevm.txt` and
    `report-evm2.txt` are identical, including the gas used in the five cases whose init code
    clears a slot the colliding address already holds. Both give a new contract empty storage, as
    the spec does on creation.
- **Cases that don't run:**
  - Pre-Osaka forks and `PragueToOsakaAtTime15k`, because yevm implements only Osaka.
  - The `blockchain_tests_engine*` formats, which repeat the same cases through the Engine
    API.
- **Fixtures:** EEST v5.4.0 is the final Osaka release. Its `fixtures_stable` was filled only
  up to Prague, so it has no Osaka cases; that is why the suite uses `fixtures_develop`.

## Legacy: ethereum/tests GeneralStateTests, Cancun (yevm-test harness, 2026-05)

```
## Run regular unit tests
cargo test

## Run GeneralStateTests suite (release)
cargo test -p yevm-test --release --tests eth -- --ignored
```

```
=== GeneralStateTests/Cancun ===
passed: 18858
failed: 58
 TOTAL: 18916

<snip>

   9 stRandom2
   8 stPreCompiledContracts2
   6 stExtCodeHash
   5 stBadOpcode
   4 stRevertTest
   4 stRandom
   3 stStaticCall
   3 stSpecialTest
   3 stArgsZeroOneBalance
   2 stSystemOperationsTest
   2 stQuadraticComplexityTest
   2 stMemoryTest
   2 stMemoryStressTest
   2 stCreateTest
   1 stTransactionTest
   1 stSStoreTest
   1 stInitCodeTest
```

99.69% of assertions complete across 2538 test suites.

---

Remaining edge cases:

- call.to must be Optional to distinguish calls/transfers to 0x0 from CREATE transactions

- gas accounting edge cases (overflow checks, drain on revert etc)

- value transfer edge cases (self-transfer, reverted transfer, etc)

- ...

---

For 12 days (less than 2 weeks!) of development I'd say it is "good enough for now", shadowing live mainnet transactions might give better insights about what is enough.
