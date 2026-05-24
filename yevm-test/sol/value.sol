// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

contract Vault {
    mapping(address => uint) private balance;

    function move(address to, uint amount) external {
        require(balance[msg.sender] >= amount, "amount");
        balance[msg.sender] -= amount;
        balance[to] += amount;
    }

    function give() external payable {
        balance[msg.sender] += msg.value;
    }

    function take(uint amount) external {
        require(balance[msg.sender] >= amount, "amount");
        balance[msg.sender] -= amount;
        (bool ok,) = msg.sender.call{gas: 2100, value: amount}("");
        require(ok, "call");
    }

    function have(address at) public view returns (uint) {
        return balance[at];
    }

    function self() public view returns (uint) {
        return balance[msg.sender];
    }
}
