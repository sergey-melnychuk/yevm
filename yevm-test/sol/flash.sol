// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

// Flash loan: borrow ETH within a single transaction, repay before it ends.

interface IBorrower {
    function onFlash(uint amount) external payable;
}

contract Flash {
    uint transient private locked;

    receive() external payable {}

    function loan(address borrower, uint amount) external {
        require(locked == 0, "reentrant");
        locked = 1;
        uint floor = address(this).balance - amount;
        IBorrower(borrower).onFlash{value: amount}(amount);
        require(address(this).balance >= floor + amount, "repay");
        locked = 0;
    }
}

// Minimal borrower: receives the loan, then repays it back.
// In a real use-case the borrowed funds would be put to work between
// onFlash() and the repayment (e.g. liquidation, swap, collateral swap).
contract MockBorrower is IBorrower {
    Flash private immutable lender;

    constructor(Flash _lender) { lender = _lender; }

    // Fund this contract first so it can repay.
    receive() external payable {}

    function onFlash(uint amount) external payable override {
        require(msg.sender == address(lender), "lender only");

        // ... use funds here ...

        // Repay: send back exactly what was borrowed.
        (bool ok,) = payable(msg.sender).call{value: amount}(abi.encode(amount));
        require(ok, "payback");
    }
}

// A full price-arbitrage bot would additionally need:
//   - DEX interfaces (e.g. Uniswap v2/v3, Curve) to execute swaps
//   - On-chain price queries or TWAP oracles to detect the price gap
//   - Profit check: assert proceeds > loan + gas cost before executing
//   - Slippage / deadline guards on every swap call
//   - Off-chain monitoring (MEV bot / keeper) to trigger loan() at the right moment
