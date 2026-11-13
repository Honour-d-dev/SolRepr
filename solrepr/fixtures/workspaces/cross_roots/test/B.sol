// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {A} from "../src/A.sol";

library B {
    function g() internal pure returns (uint256) {
        return 1;
    }
}
