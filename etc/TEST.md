```
## Run regular unit tests
cargo test -- --skip eth

## Run GeneralStateTests suite (release)
cargo test --release -p yaevmi-test eth
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
