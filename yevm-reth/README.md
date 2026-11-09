# yevm-reth — reading state from a local reth node

`yevm-reth` implements yevm's `Chain` trait by reading a local reth MDBX datadir directly
instead of making JSON-RPC calls. `replay --with-reth <datadir>` (built with
`--features reth`) uses it. The datadir is opened **read-only**, so it is safe to run next
to the live node.

This guide covers how the node must be configured and operated for this to work well.

`yevm-reth` also builds `node`: the standard reth Ethereum node with yevm as its EVM (the `exec`
module), so block validation, payload building and `eth_call`-style RPCs run on yevm. It takes
reth's usual CLI and can replace the reth binary on an existing datadir:

```
cargo build --release -p yevm-reth --bin node
target/release/node node --chain mainnet --datadir <dir> ...
```

yevm implements only the Osaka rules, so a fresh node must start with `--snap.v2` so that
only blocks after the snap target are executed. Only the `node` command uses yevm. The CLI
commands that execute blocks on their own (`re-execute`, `stage run`, `import`) still run
evm2. See the `exec` module docs for the other limitations.

## 1. What yevm can read, and how far behind the head it is

reth ≥ 2.10 persists blocks in batches, and by default it delays the state writes for the
newest blocks in each batch (*state masking*). On a `storage_v2` datadir (state is kept
only in hashed tables), state is exact on disk only up to `FinishCheckpoint::partial_state_trie`.
yevm calls this the **state frontier** (`RethDb::state_frontier()`):

```
chain head (RPC)  >=  persisted tip (best_block_number)  >=  state frontier
                      block data readable up to here         state readable up to here
```

- `RethDb::pin(n)` (state before block `n`) works for `n - 1` from the state frontier back to
  the oldest block still within the node's retained history. On a `--minimal` node that is
  ~10064 blocks (~1.4 days).
- `replay --with-reth` treats "latest" as `min(state_frontier + 1, RPC tip)`, and `watch`
  follows that. The pre-state comes from the datadir, but the block body is still fetched over
  RPC, so the block has to exist there too. With `--engine.persistence-threshold 0` the
  frontier can reach the chain head.
- Never read state with `ProviderFactory::latest()` on reth ≥ 2.10. For keys touched in the
  masked range it returns stale values that belong to no single block.

How far the frontier trails the head depends on three engine flags:

| flag | default | meaning |
|---|---|---|
| `--engine.persistence-threshold` (T) | 50 | flush once more than T canonical blocks are in memory |
| `--engine.memory-block-buffer-target` (B) | min(T, 5) | each flush persists up to `head − B` |
| `--engine.num-state-masking-blocks` (M) | 30 (forced to 0 when T = 0) | newest M persisted blocks get their state deferred |

```
frontier lag behind head  ∈  [B + M,  T + 1 + M]  blocks
flush cadence             =   every T + 1 − B blocks
```

reth validates these at startup: `B ≤ T`; `M = 0` or `M + B < T`; and
`--engine.persistence-backpressure-threshold` (default `max(16, 2T)`) `> T`.
**Lowering T without also passing `--engine.num-state-masking-blocks 0` fails validation**
unless T = 0.

## 2. Recommended node settings

| profile | flags | frontier lag | DB commits | reorg buffer |
|---|---|---|---|---|
| reth default | *(none)* | 35–81 blocks (~7–16 min) | every 46 blocks | 5 blocks |
| **recommended for `replay watch`** | `--engine.persistence-threshold 2 --engine.num-state-masking-blocks 0` | 2–3 blocks (~24–36 s) | every block | 2 blocks |
| minimum lag | `--engine.persistence-threshold 0` | 0–1 blocks | every block | none |
| batched / lighter writes | `--engine.persistence-threshold 8 --engine.num-state-masking-blocks 0` | 5–9 blocks (~1–2 min) | every 4 blocks | 5 blocks |

The **recommended** profile keeps a small in-memory buffer, so shallow reorgs (1–2 blocks,
the common case on mainnet) never touch disk, while staying within about half a minute of
the head. `persistence-threshold 0` gives the smallest lag, but every reorg then means a
disk unwind. The trade-off in every non-default profile is more frequent, smaller MDBX
commits, and no masking of hot keys (more write amplification). That is close to how
reth ≤ 2.5 behaved (threshold 7, no masking) and is fine for a `--minimal` node on NVMe.

In `start.sh`, add the flags to the `node` command:

```sh
nohup ./bin/silt-exex node \
    --datadir /home/work/Temp/reth-node/datadir \
    --chain mainnet \
    --minimal \
    --engine.persistence-threshold 2 \
    --engine.num-state-masking-blocks 0 \
    ...
```

Environment variables work too: `RETH_ENGINE_PERSISTENCE_THRESHOLD=2`,
`RETH_ENGINE_NUM_STATE_MASKING_BLOCKS=0`.

## 3. Applying changes / restarting the node

1. `./stop.sh`. It sends SIGTERM and waits up to `STOP_TIMEOUT` (300 s). On a clean
   shutdown reth's engine persists **all** in-memory blocks through the head with no
   masking, so the datadir is left fully consistent.
2. Edit `start.sh`, then run `./start.sh`.
3. Check `reth.log` for `Loaded storage settings`; the line must not show errors.

If the node was killed or crashed instead, the datadir may be left with
`partial_state_trie < Finish`. On the next start reth unwinds the trie to the partial
frontier and re-executes the rest. This is automatic but takes a few minutes. Don't
`kill -9` the node unless `stop.sh` times out.

## 4. Checking that it works

`verify` compares yevm's read path against the node's RPC **at the state frontier**, and
prints the three heights from section 1:

```sh
RETH_DATADIR=/home/work/Temp/reth-node/datadir YEVM_RPC_URL=http://127.0.0.1:8545/ \
  cargo run --release -p yevm-reth --bin verify -- 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc 8
```

(Uniswap V2 USDC/WETH pair, slot 8 = packed reserves. It changes on almost every block, so
a stale read shows up immediately.) Expect `match: true` / `acc match: true`. The frontier/persisted/RPC tips should be within
the lag range of your profile.

**`replay --with-reth` forces `--skip-check`.** Its `N/N OK` lines only mean that every
transaction executed without error. Nothing is compared with receipts or revm. To check
correctness, replay the same block in RPC mode against the local node. Both yevm and revm
then read state over RPC, and results are checked against receipts:

```sh
YEVM_RPC_URL=http://127.0.0.1:8545/ ./target/release/replay <block>
```

## 5. Upgrading reth (node and yevm-reth must match)

The node writes the on-disk format and `yevm-reth` decodes it, so their reth versions must
match. A mismatch shows up as decode panics, e.g.
`deserializing FinishCheckpoint requires the partial-persistence feature`.

1. Find the node's version: `./bin/silt-exex --version` (note the commit SHA). Ignore
   `~/.cargo/bin/reth`, which is an unrelated old install.
2. Set the same tag on every `reth-*` dependency in `yevm-reth/Cargo.toml`.
3. **Re-sync the `[patch]` sections** in the workspace `Cargo.toml` from reth's own
   workspace `Cargo.toml` at that tag (currently `evm2`/`evm2-inspectors` and the
   `reth-core` crates). Cargo doesn't apply a dependency's patches; if these are stale you
   get errors like `Account: From<&evm2::AccountInfo> is not satisfied`.
   Patches that aren't used in yevm's graph (e.g. `evm2-jit`, `inkwell`) can be left out.
4. `cargo update -p reth-ethereum`, then build. Fix API drift in `src/lib.rs`.
   For 2.10, `ProviderFactory::history_by_block_number` was removed, so state goes through
   `BlockchainProvider`.
5. If reth changed its `rocksdb` features, regenerate the RocksDB prebuilt (section 6).
   For 2.10, `default-features = false, features = ["lz4", "zstd"]` dropped snappy, zlib
   and bzip2, and the old prebuilt failed to link with undefined `inflate*`/`BZ2_*` symbols.
6. Run `verify` (section 4).

## 6. RocksDB prebuilt (`etc/rocksdb.sh`)

Building RocksDB from source takes ~10+ minutes, so the workspace `Cargo.toml` patches
`librocksdb-sys` to a local copy that links a prebuilt `librocksdb.a` from
`~/.local/share/rocksdb-patched-vendors/`. To regenerate it:

1. Delete the `librocksdb-sys = { path = ... }` line from `[patch.crates-io]`.
2. `cargo build --release -p yevm-test --features reth --bin replay` (compiles RocksDB).
3. `./etc/rocksdb.sh` from the workspace root. It copies the newest build's
   archives, adds the patch line back, and runs `cargo check`.
4. Rebuild.

Regenerate whenever the `librocksdb-sys` version, its enabled features, or the release
profile's `debug` setting changes. With `debug = true`, the archive grows from roughly 40 MB
to roughly 1.3 GB.

## 7. Troubleshooting

| symptom | cause | fix |
|---|---|---|
| panic decoding a reth type (`FinishCheckpoint`, …) | yevm-reth tag ≠ node version | section 5 |
| `state after block N is not exactly readable from disk yet (state frontier is F …)` | asked for state past the frontier | wait, or lower the lag (section 2) |
| `InsufficientChangesets` / history errors | block older than the retained history (~10064 blocks on `--minimal`) | pick a newer block |
| `watch` waits minutes between bursts of blocks | default engine settings (flush every 46 blocks) | section 2 |
| undefined `inflate*` / `BZ2_*` / other C symbols at link time | stale RocksDB prebuilt | section 6 |
| `From<&evm2::AccountInfo>` / duplicate-crate trait errors | stale `[patch]` sections | section 5, step 3 |
| `storage directory is currently in use` on node start | stale lock after a crash | `./stop.sh` removes stale locks |
| `result is null` in `replay --with-reth` | block body requested from RPC before that RPC has the block (yevm before the RPC-tip cap; or a load-balanced remote `YEVM_RPC_URL` whose backends disagree on the tip) | point `YEVM_RPC_URL` at the local node (`http://127.0.0.1:8545/`) |
