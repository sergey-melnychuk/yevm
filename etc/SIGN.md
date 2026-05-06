# dryrun

**"sign outcomes, not promises"**

## Reasoning

Every existing wallet shows you a summary before signing — token amounts, recipient address, estimated gas.
None show you what actually happens: which storage slots change, which contracts get called, which balances move, whether it reverts.
Users sign promises ("this tx will do X") not outcomes ("this tx does exactly this").

The Bybit hack ($1.5B, Feb 2025) happened because signers couldn't see that the tx they were approving upgraded a Safe implementation to a malicious contract.
A wallet that simulates before signing would have shown the side-effect: "implementation address changes from 0x... to 0x...". Nobody clicks sign on that.

Rabby wallet does run a simulation before signing, but the results are shallow — it checks whether the sender's balance is sufficient and flags obvious token drains, but does not expose raw storage changes. A malicious implementation upgrade (as in the Bybit case) would show no warning: the balance is unaffected, and the storage diff that matters is never surfaced to the user.

Existing wallets are non-extendable black boxes. The signing flow is closed. The only way to own the simulation step is to own the wallet.

## Architecture

```
Dapp
  │ WalletConnect v2
  ▼
dryrun (mobile app)
  ├── WalletConnect session management
  ├── yaevmi-core (via UniFFI) ← simulation engine
  │     └── full EVM execution against live chain state (RPC)
  ├── simulation UI (side-effects: storage, balances, logs, reverts)
  └── key management (BIP39/BIP32, Keychain/Keystore)
        │ user approves
        ▼
      sign + broadcast via RPC
```

## Stack

- **App**: React Native (iOS + Android from one codebase)
- **Simulation engine**: `yaevmi-core` compiled via UniFFI → Swift/Kotlin bindings
- **Dapp connectivity**: WalletConnect v2 (`@walletconnect/web3wallet`)
- **Key management**: `coins-bip39` + `coins-bip32` + `k256` (Rust)
- **Secure storage**: iOS Keychain / Android Keystore (platform-native, hardware-backed)
- **RPC**: user-configured endpoint (same as yaevmi browser tool)

## Key Management

1. Generate mnemonic (BIP39, 12 or 24 words) or import existing
2. Derive keys via BIP44 path `m/44'/60'/0'/0/0`
3. User sets PIN / enables biometric (Face ID / fingerprint)
4. Mnemonic encrypted with Argon2-derived key from PIN
5. Encrypted blob stored in Keychain (iOS) / Keystore (Android)
6. Biometric unlocks via Secure Enclave — raw key never hits app memory

Signing happens in Rust (k256), keys never cross the UniFFI boundary in plaintext.

## Simulation Flow

1. Dapp sends `eth_sendTransaction` params via WalletConnect
2. dryrun receives unsigned `{from, to, value, data, gas}`
3. yaevmi-core simulates against current chain state (RPC fetch)
4. UI shows full side-effects:
   - Storage slot changes (address, slot, old → new)
   - Balance deltas (ETH + ERC-20)
   - Emitted logs (decoded if ABI known)
   - Call tree (depth, gas, reverts)
   - Final status (success / revert reason)
5. User taps **Submit** or **Reject**
6. If Submit: sign with local key, broadcast via RPC, return tx hash to dapp

## Phases

### Phase 1 — Foundation
- [ ] `dryrun-core` Rust crate: key generation, derivation, signing
- [ ] UniFFI bindings for iOS + Android
- [ ] Secure mnemonic storage (Keychain/Keystore)
- [ ] Basic React Native app shell: generate/import wallet, show address

### Phase 2 — WalletConnect
- [ ] WalletConnect v2 pairing (generate URI, show QR)
- [ ] Session proposal handling (approve/reject dapp connection)
- [ ] `eth_sendTransaction` interception
- [ ] Forward to sign + broadcast after approval

### Phase 3 — Simulation
- [ ] yaevmi-core compiled for iOS (`aarch64-apple-ios`) and Android (`aarch64-linux-android`)
- [ ] UniFFI simulation interface: `simulate(rpc_url, tx_params) → SideEffects`
- [ ] Simulation UI: storage diffs, balance changes, call tree, logs
- [ ] Revert detection and reason decoding

### Phase 4 — Polish
- [ ] RPC endpoint configuration
- [ ] Multi-account support
- [ ] ERC-20 balance decoding in simulation output
- [ ] 4byte.directory selector resolution
- [ ] TestFlight / Play Store beta
