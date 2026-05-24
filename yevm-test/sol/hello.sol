// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

contract Hello {
    string public message = "it works!";
}

contract Owner {
    uint private value;
    address public owner;

    constructor(uint value_) {
        owner = msg.sender;
        value = value_;
    }

    function get() public view returns (uint) {
        return value;
    }

    function set(uint value_) public {
        require(msg.sender == owner, 'owner only');
        value = value_;
    }

    function odd() public view returns (bool) {
        if (value % 2 == 0) {
            return false;
        }
        return true;
    }
}
