// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

contract D {
    function f() public pure virtual returns (uint256) {
        return 1;
    }
}

struct Pair {
    uint256 a;
    uint256 b;
}
