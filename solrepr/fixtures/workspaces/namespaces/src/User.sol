// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import * as ns from "./Base.sol";

contract X is ns.D {
    function g() external pure returns (ns.Pair memory p) {
        p.a = 1;
        p.b = 2;
    }

    function h(address who) external pure returns (ns.D) {
        return ns.D(who);
    }
}

contract Y {
    ns.D private d;
}
