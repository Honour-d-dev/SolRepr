// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import * as ns from "./Base.sol";

contract BadState {
    ns private d;
}

contract BadReturn {
    function f() external pure returns (ns) {
        return ns;
    }
}

contract BadLocal {
    function local() external pure {
        ns x;
    }

    function param(ns y) external pure {}

    function array() external pure {
        ns[] z;
    }
}
