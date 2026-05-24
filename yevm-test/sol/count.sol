// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

contract Count {
    uint256 private count;

    function get() public view returns (uint256) {
        return count;
    }

    function inc() public {
        count += 1;
    }

    function dec() public {
        count -= 1;
    }

    function set(uint256 count_) public {
        count = count_;
    }
}
