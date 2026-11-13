// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {B} from "../test/B.sol";

contract A {
    function f() public pure returns (uint256) {
        return B.g();
    }
}
