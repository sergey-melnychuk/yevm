// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

contract Logic {
    uint public value;
    address public owner;

    function init(uint value_) external {
        require(owner == address(0), "already init");
        owner = msg.sender;
        value = value_;
    }

    function set(uint value_) external {
        require(msg.sender == owner, "owner only");
        value = value_;
    }

    function get() external view returns (uint) {
        return value;
    }
}

contract Proxy {
    address private impl;

    constructor(address impl_) {
        impl = impl_;
    }

    function upgrade(address impl_) external {
        impl = impl_;
    }

    receive() external payable {
        require(false, "nope");
    }

    fallback() external {
        address target = impl;
        assembly {
            calldatacopy(0, 0, calldatasize())
            let ok := delegatecall(gas(), target, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            if ok { return(0, returndatasize()) }
            revert(0, returndatasize())
        }
    }
}
