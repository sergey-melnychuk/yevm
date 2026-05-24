// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
    function transferFrom(address, address, uint256) external returns (bool);
}

contract Token {
    address private owner;
    IERC20 public token;
    uint public price;
    uint public total;
    mapping(address => bool) private valid;
    uint private seq;
    mapping(address => uint) private used;

    constructor(address _token, uint _price, uint _total) {
        token = IERC20(_token);
        price = _price;
        total = _total;
        owner = msg.sender;
    }

    function buy() external payable {
        require(total > 0, "total");
        bool ok = token.transferFrom(msg.sender, address(this), price);
        require(ok, "transfer");
        valid[msg.sender] = true;
        total -= 1;
    }

    function use() public returns (uint) {
        require(valid[msg.sender], "valid");
        seq += 1;
        valid[msg.sender] = false;
        used[msg.sender] = seq;
        return seq;
    }

    function give(address dst) external payable {
        require(valid[msg.sender], "valid");
        valid[msg.sender] = false;
        valid[dst] = true;
    }

    function check(address target) public view returns (uint) {
        require(used[target] > 0, "no receipt");
        return used[target];
    }

    function withdraw(address to) public {
        require(msg.sender == owner, "owner");
        uint balance = token.balanceOf(address(this));
        bool ok = token.transfer(to, balance);
        require(ok, "transfer");
    }
}
