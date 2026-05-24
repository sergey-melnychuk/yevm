// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

contract Caller {
    Callee private callee;
    uint private x;

    constructor(uint x_) {
        x = x_;
    }

    function create() external returns (address) {
        Callee created = new Callee();
        callee = created;
        return address(created);
    }

    function call(uint a, uint b) external view returns (uint) {
        uint ret = callee.call(a, b);
        return ret;
    }

    function callback(uint a, uint b) external view returns (uint) {
        return a + b - x;
    }
}

contract Callee {
    address private caller;

    constructor() {
        caller = msg.sender;
    }

    function call(uint a, uint b) external view returns (uint) {
        uint ret = Caller(caller).callback(a, b);
        return ret;
    }
}
