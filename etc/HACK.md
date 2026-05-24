# Bybit Hack — February 21, 2025
## The Largest Cryptocurrency Theft in History: $1.5B Lost to a Single Blind-Signed Transaction

---

## Overview

On February 21, 2025, the Bybit cryptocurrency exchange suffered the largest digital heist in recorded history.
**401,347 ETH and ~113,000 synthetic ETH tokens**, valued at approximately **$1.5 billion**, were drained from
Bybit's Ethereum cold wallet in a matter of minutes.

The root cause was not a flaw in Ethereum, not a broken cryptographic primitive, and not a misconfigured
multisig. It was a **visibility failure**: the signers could not see what they were actually signing.

---

## Key Addresses

| Role | Address |
|---|---|
| Bybit Cold Wallet (victim proxy) | `0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4` |
| Legitimate Safe masterCopy (pre-hack) | `0x34CfAC646f301356fAa8B21e94227e3583Fe3F5F` |
| Spoofing contract (ERC-20 transfer facade) | `0x96221423681A6d52E184D440a8eFCEbB105C7242` |
| Malicious implementation (post-hack masterCopy) | `0xbDd077f651EBe7f7b3cE16fe5F2b025BE2969516` |
| Attacker EOA (initiator) | `0x0fa09C3A328792253f8dee7116848723b72a6d2e` |
| Attacker consolidation wallet | `0x47666Fab8bd0Ac7003bce3f5C3585383F09486E2` |

---

## All Involved Transactions

### Phase 0 — Preparation (February 18–19, 2025)

- **February 18, 2025**: Lazarus Group deploys the malicious implementation contract at
  `0xbDd077f651EBe7f7b3cE16fe5F2b025BE2969516`. It contains two backdoor functions:
  - `sweepETH(address receiver)` — transfers entire ETH balance to receiver
  - `sweepERC20(address token, address to)` — transfers all tokens of given type to receiver
  Both functions include a hardcoded caller check, ensuring only the attacker can invoke them.

- **February 19, 2025**: Malicious JavaScript is injected into Safe{Wallet}'s AWS S3 bucket serving
  `app.safe.global`. The code activates only for Bybit's cold wallet address, leaving all other Safe
  users unaffected.

---

### Phase 1 — The Signed Transaction (Implementation Swap)

> ⚠️ **THIS IS THE TRANSACTION THAT COULD HAVE BEEN CAUGHT**

```
0x46deef0f52e3a983b67abf4714448a41dd7ffd6d32d32da69d62081c68ad7882
```

**What Bybit's signers saw (Safe UI):**
A routine transfer of 30,000 ETH to the hot wallet at `0xf89d7b9c864f589bbF53a82105107622B35EaA40`.

**What was actually signed:**

```
execTransaction(
  to:        0x96221423681A6d52E184D440a8eFCEbB105C7242,  // spoofing contract
  data:      0xa9059cbb...bdd077f651ebe7f7b3ce16fe5f2b025be29695160...0,
             // looks like transfer(address,uint256) — actually sets masterCopy
  operation: 1,   // ← DELEGATECALL (not 0 = call)
  safeTxGas: 45746
)
```

**What it actually did:**
Via `delegatecall`, the spoofing contract's code executed in the proxy's storage context.
It wrote the attacker's contract address into **storage slot 0** of the proxy — the `masterCopy` slot.

Before: `slot[0] = 0x34CfAC646f301356fAa8B21e94227e3583Fe3F5F` (legitimate Safe logic)
After:  `slot[0] = 0xbDd077f651EBe7f7b3cE16fe5F2b025BE2969516` (attacker's drain contract)

**No ETH moved. The wallet appeared intact. The attacker now owned it entirely.**

---

### Phase 2 — The Drain Transactions (No Approval Required)

All five transactions were sent unilaterally by the attacker. No multisig. No Bybit participation.
The wallet's logic was already replaced.

> 🔴 **ETH DRAIN**
```
0xb61413c495fdad6114a7aa863a00b2e3c28945979a10885b12b30316ea9f072c
```
`sweepETH` → **401,346 ETH** (~$1.08B) → `0x47666Fab8bd0Ac7003bce3f5C3585383F09486E2`

> 🔴 **stETH DRAIN**
```
0xa284a1bc4c7e0379c924c73fcea1067068635507254b03ebbbd3f4e222c1fae0
```
`sweepERC20` → **90,375 stETH** (~$242M) → attacker wallet

> 🔴 **cmETH DRAIN**
```
0x847b8403e8a4816a4de1e63db321705cdb6f998fb01ab58f653b863fda988647
```
`sweepERC20` → **15,000 cmETH** (~$42M) → attacker wallet

> 🔴 **mETH DRAIN**
```
0xbcf316f5835362b7f1586215173cc8b294f5499c60c029a3de6318bf25ca7b20
```
`sweepERC20` → **8,000 mETH** (~$22.5M) → attacker wallet

> 🔴 **USDT DRAIN**
```
0x25800d105db4f21908d646a7a3db849343737c5fba0bc5701f782bf0e75217c9
```
`sweepERC20` → **90 USDT** → attacker wallet

---

### Phase 3 — Laundering

- Stolen ETH consolidated at `0x47666Fab8bd0Ac7003bce3f5C3585383F09486E2`
- Distributed in **10,000 ETH increments** across ~40 wallets
- stETH/mETH swapped to ETH via Paraswap/Uniswap
- **83% converted to BTC** via ThorChain across 6,954 wallets
- Cross-chain bridges, DEXes, and mixers used to obscure trail
- By March 4, 2025: all ~499,000 ETH fully laundered

---

## The Kill-Chain

```
[Feb 4]   Lazarus compromises Safe developer's macOS workstation via social engineering
          (Docker project "MC-Based-Stock-Invest-Simulator-main" → getstockprice[.]com)
          AWS session tokens stolen, MFA bypassed

[Feb 18]  Malicious implementation contract deployed on Ethereum
          0xbDd077f651EBe7f7b3cE16fe5F2b025BE2969516
          Contains sweepETH() + sweepERC20() with hardcoded attacker auth

[Feb 19]  Malicious JS injected into Safe's S3 bucket
          Targets only Bybit's wallet address — all other users unaffected
          JS intercepts execTransaction proposal, swaps payload silently

[Feb 21, 14:13 UTC]
          Bybit initiates routine cold→hot wallet transfer
          Signers open Safe UI — see legitimate transfer of 30,000 ETH
          Behind the scenes: JS replaces tx data with delegatecall payload
          Three signers confirm on Ledger hardware wallets (blind signing)
          TX 0x46deef0f... broadcast → proxy slot[0] overwritten
          Bybit's cold wallet is now fully controlled by Lazarus

[Feb 21, 14:13–14:16 UTC]
          Attacker calls sweepETH(), sweepERC20() × 4
          $1.5B drained in 5 transactions, no approvals needed

[Feb 21, 14:15 UTC]
          Malicious JS removed from S3 bucket — evidence destroyed
          Safe UI returns to normal

[Feb 21, 15:20 UTC]
          ZachXBT publishes alert on Telegram
          Bybit CEO Ben Zhou confirms the hack
```

---

## Analysis: Legitimate vs Malicious Transaction

The cold wallet `0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4` had a well-established pattern of
legitimate operations. Prior historical `execTransaction` calls from this wallet consistently showed:

| Field | Legitimate Transactions | Malicious Transaction (0x46deef0f...) |
|---|---|---|
| `to` | `0xf89d7b9c864f589bbF53a82105107622B35EaA40` (known hot wallet) | `0x96221423681A6d52E184D440a8eFCEbB105C7242` (**3-day-old unknown contract**) |
| `operation` | `0` (standard call) | **`1` (delegatecall)** ← critical anomaly |
| `value` | 30,000–60,000 ETH | `0` ← no ETH transfer at all |
| `data` | `0x` (empty, pure ETH transfer) | Non-empty calldata disguised as ERC-20 transfer |
| Counterparty history | Known, repeatedly used address | **First-ever interaction** |
| Storage side-effects | None (pure value transfer) | **Overwrites proxy slot[0]** |

A January 14, 2025 transaction from the same wallet involved an ERC-20 (mETH) transfer using a
delegatecall — the one legitimate precedent. However, that transaction targeted a **known, previously
interacted-with contract**. The February 21 transaction targeted a contract deployed **72 hours prior**
with **zero prior interactions**.

Any pre-execution simulation of the malicious transaction would have produced the following
observable state diff:

```
Storage write detected:
  Contract: 0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4
  Slot:     0x0000000000000000000000000000000000000000000000000000000000000000
  Before:   0x00000000000000000034CfAC646f301356fAa8B21e94227e3583Fe3F5F
  After:    0x000000000000000000bDd077f651EBe7f7b3cE16fe5F2b025BE2969516

Implementation replaced. Wallet logic is now controlled by an unknown contract
deployed 3 days ago with no prior interactions.
```

This is not subtle. This is a complete proxy takeover, visible in full detail in the execution trace.

---

## How Pre-Execution Side-Effect Attestation Would Have Prevented This

### The Problem: Blind Signing

The signers were presented with a UI showing a routine transfer. Their Ledger devices signed raw
transaction bytes they could not interpret. The gap between "what is displayed" and "what is executed"
is the entire attack surface.

### The Solution: Pre-Execution Simulation

A pre-signing EVM simulation runs the transaction against a fork of current chain state and produces
a complete, human-readable attestation of every side-effect before any signature is collected.

For the malicious transaction `0x46deef0f...`, such a simulation would have produced:

```
=== TRANSACTION SIMULATION REPORT ===

Initiator:  0x0fa09C3A328792253f8dee7116848723b72a6d2e
Target:     0x96221423681A6d52E184D440a8eFCEbB105C7242

⚠️  WARNING: DELEGATECALL detected (operation=1)
⚠️  WARNING: Target contract deployed 3 days ago — never previously interacted with
⚠️  WARNING: No ETH transferred despite routine transfer intent

STORAGE MUTATIONS:
  [0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4]
    slot[0]: 0x34CfAC...3F5F → 0xbDd077...9516
    ↑ PROXY IMPLEMENTATION REPLACED

POST-EXECUTION STATE:
  Wallet 0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4 now executes code from:
  0xbDd077f651EBe7f7b3cE16fe5F2b025BE2969516

  This contract exposes:
    sweepETH(address receiver)     — transfers ALL ETH to receiver
    sweepERC20(address token, address to) — transfers ALL tokens to receiver
  Both callable by: 0x47666Fab8bd0Ac7003bce3f5C3585383F09486E2 (unknown EOA)

VERDICT: ❌ REJECT — This transaction transfers ownership of the wallet.
         Current balance at risk: 401,346 ETH + 113,000 stETH/mETH/cmETH
```

**No signer would have approved this.**

### Why This Works Even Against a Compromised UI

The critical property is that simulation operates on raw transaction bytes — the same bytes sent to
the hardware wallet — not on the UI representation. Even if the Safe frontend displays a benign
transfer, the simulation runs the actual payload and reports actual storage mutations.

The UI lie becomes irrelevant because the ground truth is the EVM state transition, not the HTML
rendered to the signer.

### The Attestation Model

Pre-execution side-effect attestation requires:

1. **Simulation** — run the exact calldata against a forked chain state
2. **Diff extraction** — enumerate all storage writes, ETH balance changes, log emissions
3. **Semantic analysis** — flag anomalies: unexpected contracts, slot[0] writes on proxy contracts,
   delegatecalls to unknown targets, zero-value transfers with non-empty calldata
4. **Human-readable report** — presented to signers before any signature is collected
5. **Policy enforcement** — optionally block signing if anomalies exceed threshold

Steps 1–3 are exactly what an EVM execution trace provides. The implementation is a matter of
connecting trace output to a signing policy engine.

### This Is Precisely What YEVM Enables

YEVM (Yet Another EVM Implementation) produces full execution traces — opcode-level, with complete
storage read/write records — in-browser, against any RPC-accessible chain state. No backend, no
trusted third party, no additional attack surface.

For the Bybit scenario:

- Load `0x46deef0f...` calldata
- Fork state at block preceding the transaction
- Execute
- Observe: `SSTORE` at slot 0 of the victim proxy — implementation replaced
- Flag: target contract is 3 days old, never interacted with
- Report: full state diff to signers before signature collection

**$1.5 billion would still be in Bybit's cold wallet.**

---

## Aftermath

- Bybit covered all losses 1:1 from internal funds and emergency loans (Galaxy Digital, Wintermute, FalconX)
- ~447,000 ETH acquired to restore reserves
- Hacken proof-of-reserves audit confirmed full collateralization by February 24, 2025
- Safe{Wallet} patched: signature/hash verification now enforced server-side before saving proposals
- Bybit announced development of proprietary wallet infrastructure — away from Safe
- The compromised wallet `0x1Db92e2EeBC8E0c075a02BeA49a2935BcD2dFCF4` remains on-chain, empty,
  its proxy still pointing to the attacker's implementation contract. It is abandoned.
- Lazarus Group laundered ~100% of stolen funds by March 4, 2025. Recovery probability: near zero.

---

## References

- NCC Group: https://www.nccgroup.com/research/in-depth-technical-analysis-of-the-bybit-hack/
- CertiK: https://www.certik.com/resources/blog/bybit-incident-technical-analysis
- Sygnia: https://www.sygnia.co/blog/sygnia-investigation-bybit-hack/
- Chainalysis: https://www.chainalysis.com/blog/bybit-exchange-hack-february-2025-crypto-security-dprk/
- Blockaid: https://www.blockaid.io/blog/the-15b-bybit-hack-explained-a-technical-breakdown
- Dfns: https://www.dfns.co/article/the-bybit-safe-hack
- AnChain.AI: https://www.anchain.ai/blog/bybit
- AMLCrypto (full tx list): https://amlcrypto.io/en/blog/event-chronology-bybit-hack
- Etherscan — victim wallet: https://etherscan.io/address/0x1db92e2eebc8e0c075a02bea49a2935bcd2dfcf4
- Etherscan — impl swap tx: https://etherscan.io/tx/0x46deef0f52e3a983b67abf4714448a41dd7ffd6d32d32da69d62081c68ad7882
- Etherscan — ETH drain tx: https://etherscan.io/tx/0xb61413c495fdad6114a7aa863a00b2e3c28945979a10885b12b30316ea9f072c
