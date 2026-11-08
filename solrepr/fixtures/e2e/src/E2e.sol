// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract E2e {
    uint256 public amount;

    function add(uint256 x) external view returns (uint256) {
        return x + amount;
    }
}
