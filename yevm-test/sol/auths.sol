// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

// Meta-transaction executor: an off-chain signer authorises a call by signing a hash;
// any relayer can submit it and pays the gas while the call executes as the signer.
//
// Follows EIP-191 version 0x45 ("\x19Ethereum Signed Message:\n32").
// For structured typed data with domain separation, see EIP-712.

contract Auth {
    // Per-signer nonce; consumed on every successful execution to block replay.
    mapping(address => uint) public nonces;

    event Executed(address indexed signer, address indexed target, uint nonce);

    // Execute `data` on `target` as if called by `signer`.
    // `sig` is a 65-byte ECDSA signature (r || s || v) over the EIP-191 hash of
    // abi.encode(address(this), target, data, nonce).
    function execute(
        address signer,
        address target,
        bytes calldata data,
        uint nonce,
        bytes calldata sig
    ) external returns (bytes memory result) {
        require(nonces[signer] == nonce, "bad nonce");
        require(sig.length == 65, "bad sig length");

        bytes32 hash = _digest(target, data, nonce);
        address recovered = ecrecover(hash, uint8(sig[64]), _b32(sig, 0), _b32(sig, 32));
        require(recovered != address(0) && recovered == signer, "bad sig");

        nonces[signer]++;

        bool ok;
        (ok, result) = target.call(data);
        require(ok, "call failed");

        emit Executed(signer, target, nonce);
    }

    // Returns the exact bytes32 the signer must hash-and-sign off-chain.
    // external so it can't be called internally — _digest exists solely for that.
    function digest(address target, bytes calldata data, uint nonce) external view returns (bytes32) {
        return _digest(target, data, nonce);
    }

    function _digest(address target, bytes memory data, uint nonce) internal view returns (bytes32) {
        bytes32 inner = keccak256(abi.encode(address(this), target, data, nonce));
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", inner));
    }

    // Read 32 bytes from a calldata slice at byte offset `off`.
    function _b32(bytes calldata b, uint off) private pure returns (bytes32 r) {
        assembly { r := calldataload(add(b.offset, off)) }
    }
}

// Minimal call target: records what was set and who called it.
// When invoked via Auth.execute() the caller will be the Auth contract address,
// not the original relayer — illustrating the delegation model.
contract Box {
    uint public value;
    address public lastCaller;

    function set(uint v) external {
        value = v;
        lastCaller = msg.sender;
    }
}
