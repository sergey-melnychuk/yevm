# detect: propAMM swap & quote detection

`detect` streams mainnet blocks, re-executes every transaction with yevm, and
runs two analyses over the trace stream:

- **swap detection** (`yevm-lens::analyse`) — reconstructs token swaps from
  confirmed state changes;
- **quote tracking** (`yevm-lens::quotes::QuoteTracker`) — links each propAMM
  swap to the maker's quote-update tx it executed against.

## Principle: state changes, not logs

Logs are cheap to fake — any contract can emit any topic. Everything here is
*detected* from state changes and execution structure:

| signal | source |
|---|---|
| ERC-20 transfer | `Transfer` log **cross-checked against a balance-slot write** (keccak preimage of `mapping(address=>uint)`, Solidity and Vyper layouts); unconfirmed logs are discarded (and surfaced as `forged_transfers`) |
| native ETH leg | call values (`Call.eth` on CALL/CREATE frames), cancelled when the frame reverts |
| WETH wrap/unwrap | Deposit/Withdrawal confirmed by the wrapper's balance writes |
| swapper vs pool | call-stack ancestry + entry order + "executed code in its own frame" |
| quote update / link | raw storage writes (`Put`) and reads (`Get`), matched slot-by-slot |
| venue (`via:` line) | `Call` targets actually entered during the tx |

Swap logs (UniV2/V3/V4 topics) are used **only to label** the protocol of a
pool already verified by flows; an emitter with no backing flow is reported in
`unverified_swaps`, never as a swap.

## Swap detection

1. Confirmed flows are netted per `(holder, token)`.
2. Every account that net-converted one token into another is a *candidate* —
   that shape fits both a pool and a contract-held swapper.
3. The call graph tells them apart: a swapper initiates calls into its pools, a
   pool is on the receiving end. Ties on call cycles (V3/V4 callbacks) resolve
   by entry order. Passive accounts (no code executed in their own frame) and
   the tx sender are never pools.
4. One `Swap` per swapper, plus a payer/recipient pairing for one-sided ends:
   - sender-anchored: the sender's end-to-end swap even when the output lands
     at a different recipient (fee skims lose to the largest single inflow);
   - relayed (ERC-4337 bundles, meta-txs): when the fee payer is a bundler, the
     largest sold-only account pairs with the largest bought-only account,
     restricted to tokens the route actually converts; gas refunds, bribes and
     the coinbase are excluded, and both ends must exist.

### Works well

- end-to-end swaps through routers/aggregators, incl. output to a different
  recipient (verified against Etherscan for
  `0x7e3d3abe9178f945e54cf546261d56f682b0fc10746a68c0f9f31d99e50aec9b`);
- RFQ/propAMM maker fills: the passive inventory wallet is reported as its own
  swap (counterparty view), without inheriting the route's pools;
- ERC-4337 bundles: taker pair recovered although the tx sender is a bundler
  and the smart wallet nets zero (verified against
  `0x81e12c4440bd69af9d777245273f3213bdd05821d9f07b868bcabcfcb55b2e6e`);
- multi-leg routes with native ETH legs (ETH sentinel `0xeeee…eeee`), wrap hops
  appear as pools by design;
- spoofed `Transfer`/`Swap` logs cannot create swaps (no balance write, no
  flow).

### Falls short

- **amounts** come from the log payloads of confirmed transfers, not from the
  balance deltas themselves — fee-on-transfer tokens report pre-fee amounts;
- **coinbase bribes** paid as ETH call values count toward a swapper's net ETH
  outflow (sold side slightly overstated);
- **pure-profit arbitrage** is not a swap: a bot that nets only profit (e.g.
  flash-borrows WETH, fills against a maker, repays) has no net conversion, so
  only the maker's side is reported — see the fills in block 26096770;
- **relayed pairing picks by raw amount** across tokens with different
  decimals; a bundle with several unrelated user swaps can mispair the ends
  (walking flows through zero-net conduits would fix this);
- a **passive maker's `pools` are usually empty** and protocol `Unknown` — the
  amm it filled through moves no tokens itself; the `via:` line (from Call
  targets) names the venue instead;
- **protocol labels** exist only for recognized topics (UniV2/V3/V4); other
  venues show `Unknown` even when real;
- **reverted swaps** are discarded with the reverted state (detecting attempted
  swaps is a TODO in detect.rs).

## Quote tracking (propAMM)

A proprietary AMM holds no curve liquidity: makers stream signed quote-update
txs to the builder (Titan), which places the latest applicable update
**immediately before the taker's tx, in the same block**
(<https://docs.titanbuilder.xyz/propamms>). On-chain:

- a **quote update** writes a quote store's storage and moves nothing — no
  swaps, no transfers, no approvals (approvals excluded because approve→swap
  would otherwise look like update→consume);
- the **swap reads those exact slots** (the amm STATICCALLs the store for the
  live price), yielding a `QuoteLink` back to the update tx and its maker.

The tracker indexes storage writes of **every** transaction — quote stores are
*discovered* by the slot match, never listed in advance. This matters: besides
Titan's shared store `0xda7afeed…` ("data feed"), makers run their own (e.g.
`0x0109aa91…` serving the `0x585d44…` inventory wallet), and new ones appear
without notice. Old writes are evicted on a rolling window (`RETAIN_BLOCKS`,
64) — same-block updates make even a short window lossless.

### Works well

- update→consume pairs link with slot-level precision, maker and age included;
  verified on-chain (written slots ∩ read slots) for multiple pairs, e.g.
  block 26095890 `0x6042943e…` → `0x8892d052…`, and the `0x0109aa91…` store in
  block 26096770 (txs 106→107, 308→309);
- swap txs' own storage writes are indexed but never count as quote updates, so
  a swap cannot link to a previous swap or to itself;
- unknown stores surface in the `quote consumed:` line by address (`?` name) —
  add them to the lookup to also print their update txs as they land.

### Falls short

- a fill against a quote **older than the retention window** (or pushed before
  the process started) does not link — by observation updates land in the same
  block, so this is rare;
- the "no token movement" heuristic admits other state pushes: an **oracle
  update** (e.g. Chainlink transmit) read by a swap links as a consumed quote —
  arguably true ("price data consumed"), but it is not a pAMM maker;
- the update's **maker** is the tx fee payer — an ops wallet, not necessarily
  the economic maker; the quote store verifies the real maker's signature in
  calldata, which is not parsed;
- **approvals are recognized from logs** (`Approval` has no balance write to
  confirm against), so this one exclusion is log-trusting;
- **reorgs**: `purge_from` exists but is not wired into the reorg path of
  detect.rs (demo); a reorged update could briefly produce a stale link;
- update txs print as `quote update:` only for feeds named in the lookup —
  unnamed stores are indexed silently and only revealed when consumed.

## Known addresses

The propAMM router and amm instances come from
<https://github.com/lambdaclass/propamm-router-contracts> (the table is stale:
pools `0xb09999a4…`, `0xc9a956b5…`, `0xb09aaa89…`, `0x3ce2672a…` were observed
filling but are not listed). Quote stores known so far: `0xda7afeed…` (Titan,
shared), `0x0109aa91…` (maker-run). Verified update→swap pairs are kept in the
comment block at the bottom of `src/bin/detect.rs`.
