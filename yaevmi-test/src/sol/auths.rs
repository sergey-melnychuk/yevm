use k256::ecdsa::{SigningKey, signature::hazmat::PrehashSigner};
use yaevmi_base::{Acc, Int, acc, int};
use yaevmi_core::{Call, Head, Tx, state::Account};
use yaevmi_misc::buf::Buf;

// --- helpers ---

/// CREATE address: keccak256(rlp([sender, nonce]))[12..]
fn create_addr(sender: Acc, nonce: u8) -> Acc {
    // RLP: nonce 0 encodes as 0x80 (empty); 1..=127 encodes as the byte itself
    let nonce_rlp: &[u8] = if nonce == 0 { &[0x80] } else { &[nonce] };
    let payload = 1 + 20 + nonce_rlp.len(); // 0x94 prefix + 20-byte addr + nonce
    let mut rlp = Vec::with_capacity(1 + payload);
    rlp.push(0xc0 + payload as u8); // list prefix
    rlp.push(0x94); // 0x80 + 20
    rlp.extend_from_slice(sender.as_ref());
    rlp.extend_from_slice(nonce_rlp);
    Acc::from(&yaevmi_misc::keccak256(&rlp).as_ref()[12..])
}

/// Ethereum address from a k256 signing key: keccak256(uncompressed_pubkey[1..])[12..]
fn addr_of(key: &SigningKey) -> Acc {
    let point = key.verifying_key().to_encoded_point(false);
    Acc::from(&yaevmi_misc::keccak256(&point.as_bytes()[1..]).as_ref()[12..])
}

/// u64 right-aligned in a 32-byte word (ABI uint256 encoding).
fn word(v: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[24..].copy_from_slice(&v.to_be_bytes());
    b
}

/// Address left-padded to 32 bytes (ABI address encoding).
fn addr_word(a: &Acc) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(a.as_ref());
    b
}

/// Mirrors Auth._digest on-chain.
/// inner  = keccak256(abi.encode(auth, target, data, nonce))
/// digest = keccak256("\x19Ethereum Signed Message:\n32" ++ inner)
fn make_digest(auth: &Acc, target: &Acc, data: &[u8], nonce: u64) -> Int {
    let data_pad = data.len().div_ceil(32) * 32;
    let mut enc = Vec::with_capacity(4 * 32 + 32 + data_pad);
    enc.extend_from_slice(&addr_word(auth)); // address(this)
    enc.extend_from_slice(&addr_word(target)); // target
    enc.extend_from_slice(&word(4 * 32)); // offset to `data` = 128
    enc.extend_from_slice(&word(nonce)); // nonce
    enc.extend_from_slice(&word(data.len() as u64)); // data.length
    enc.extend_from_slice(data);
    enc.resize(enc.len() + data_pad - data.len(), 0); // pad to 32-byte boundary

    let inner = yaevmi_misc::keccak256(&enc);

    let mut prefixed = b"\x19Ethereum Signed Message:\n32".to_vec();
    prefixed.extend_from_slice(inner.as_ref());
    yaevmi_misc::keccak256(&prefixed)
}

/// ABI-encode execute(address signer, address target, bytes data, uint256 nonce, bytes sig).
/// Heads: signer(32) + target(32) + data_offset(32) + nonce(32) + sig_offset(32) = 160 bytes
fn encode_execute(signer: &Acc, target: &Acc, data: &[u8], nonce: u64, sig: &[u8; 65]) -> Vec<u8> {
    let sel = yaevmi_misc::keccak256(b"execute(address,address,bytes,uint256,bytes)");
    let data_pad = data.len().div_ceil(32) * 32;
    let sig_pad = 65usize.div_ceil(32) * 32; // = 96
    let data_off = 5u64 * 32; // 5 head slots × 32 = 160
    let sig_off = data_off + 32 + data_pad as u64; // past data-length + data-content

    let mut out = sel.as_ref()[..4].to_vec();
    out.extend_from_slice(&addr_word(signer));
    out.extend_from_slice(&addr_word(target));
    out.extend_from_slice(&word(data_off));
    out.extend_from_slice(&word(nonce));
    out.extend_from_slice(&word(sig_off));
    out.extend_from_slice(&word(data.len() as u64));
    out.extend_from_slice(data);
    out.resize(out.len() + data_pad - data.len(), 0);
    out.extend_from_slice(&word(65));
    out.extend_from_slice(sig);
    out.resize(out.len() + sig_pad - 65, 0);
    out
}

fn test_head() -> Head {
    Head {
        number: 1.into(),
        hash: int("0x1"),
        gas_limit: 1_000_000.into(),
        coinbase: acc("0xC014BA5E"),
        timestamp: 42.into(),
        base_fee: 1.into(),
        blob_base_fee: Some(1.into()),
        blobhash: Some(Int::ONE),
        prevrandao: Int::ONE,
        parent_hash: int("0x1"),
        parent_beacon_block_root: None,
    }
}

// --- test ---

#[tokio::test]
async fn test_meta_tx() -> eyre::Result<()> {
    let combined = super::load()?;
    let auth_src = &combined.contracts["sol/auths.sol:Auth"];
    let box_src = &combined.contracts["sol/auths.sol:Box"];

    let relayer = acc("0xBB");

    // Deterministic test key — never use outside tests.
    let signer_key = SigningKey::from_bytes(&[0x42u8; 32].into()).unwrap();
    let signer = addr_of(&signer_key);

    // Contract addresses are deterministic from deployer + nonce.
    let auth_addr = create_addr(relayer, 0);
    let box_addr = create_addr(relayer, 1);

    // Signer builds Box.set(99) calldata off-chain.
    let set_sel = yaevmi_misc::keccak256(b"set(uint256)");
    let mut set_data = set_sel.as_ref()[..4].to_vec();
    set_data.extend_from_slice(&word(99));

    // Signer computes the Auth digest and signs it.
    let digest = make_digest(&auth_addr, &box_addr, &set_data, 0 /* nonce */);
    let (sig, rec_id) = signer_key.sign_prehash(digest.as_ref()).unwrap();
    let mut sig65 = [0u8; 65];
    sig65[..64].copy_from_slice(&sig.to_bytes()); // r (32) || s (32)
    sig65[64] = rec_id.to_byte() + 27; // v = 27 or 28

    let auth_hash = yaevmi_misc::keccak256(&auth_src.bin_runtime.0);
    let box_hash = yaevmi_misc::keccak256(&box_src.bin_runtime.0);

    // Pre-populate state: Auth and Box already deployed at their deterministic addresses.
    let env = || {
        vec![
            (
                relayer,
                Account {
                    value: super::ethers(1),
                    nonce: Int::ZERO,
                    code: (Buf::default(), Int::ZERO),
                },
                vec![],
            ),
            (
                auth_addr,
                Account {
                    value: Int::ZERO,
                    nonce: Int::ZERO,
                    code: (auth_src.bin_runtime.clone(), auth_hash),
                },
                vec![],
            ),
            (
                box_addr,
                Account {
                    value: Int::ZERO,
                    nonce: Int::ZERO,
                    code: (box_src.bin_runtime.clone(), box_hash),
                },
                vec![],
            ),
        ]
    };

    // --- valid signature ---
    // Relayer submits execute(): Auth verifies sig, forwards Box.set(99).
    let call_data = encode_execute(&signer, &box_addr, &set_data, 0, &sig65);
    let call = Call {
        by: relayer,
        to: Some(auth_addr),
        gas: 500_000,
        eth: Int::ZERO,
        data: Buf(call_data),
    };
    let tx = Tx {
        chain_id: 1.into(),
        nonce: 0.into(),
        gas_price: 1.into(),
        max_fee_per_gas: 1.into(),
        max_priority_fee_per_gas: 1.into(),
        access_list: vec![],
        authorization_list: vec![],
        blob_versioned_hashes: vec![],
        max_fee_per_blob_gas: Some(1.into()),
        hash: Int::ZERO,
        index: Int::ZERO,
    };

    let res = super::run(call.clone(), test_head(), env(), tx.clone()).await?;
    assert_eq!(res.0, Int::ONE, "valid meta-tx must succeed");

    // --- corrupted signature ---
    // Flipping a byte in r makes ecrecover recover the wrong address → "bad sig" revert.
    let mut bad_sig = sig65;
    bad_sig[0] ^= 0xff;
    let bad_data = encode_execute(&signer, &box_addr, &set_data, 0, &bad_sig);
    let bad_call = Call {
        data: Buf(bad_data),
        ..call.clone()
    };

    let (bad_status, ..) = super::run(bad_call, test_head(), env(), tx.clone()).await?;
    assert_eq!(bad_status, Int::ZERO, "corrupted signature must revert");

    // --- wrong nonce ---
    // Signer signs nonce=1 but contract nonces[signer] is 0 → "bad nonce" revert.
    let digest1 = make_digest(&auth_addr, &box_addr, &set_data, 1);
    let (sig1, rec1) = signer_key.sign_prehash(digest1.as_ref()).unwrap();
    let mut sig65_1 = [0u8; 65];
    sig65_1[..64].copy_from_slice(&sig1.to_bytes());
    sig65_1[64] = rec1.to_byte() + 27;
    let nonce1_data = encode_execute(&signer, &box_addr, &set_data, 1, &sig65_1);
    let nonce1_call = Call {
        data: Buf(nonce1_data),
        ..call
    };

    let (nonce1_status, ..) = super::run(nonce1_call, test_head(), env(), tx.clone()).await?;
    assert_eq!(nonce1_status, Int::ZERO, "wrong nonce must revert");

    Ok(())
}
