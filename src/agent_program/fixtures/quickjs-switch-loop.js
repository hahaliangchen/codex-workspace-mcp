// Adapted from QuickJS tests/test_loop.js, test_switch2.
// Copyright (c) 2017-2021 Fabrice Bellard and Charlie Gordon.
// See quickjs-LICENSE.txt for the MIT license.
function main() {
    var i, a, s;
    s = "";
    for (i = 0; i < 4; i++) {
        a = "?";
        switch (i) {
        case 0:
            a = "a";
            break;
        case 1:
            a = "b";
            break;
        case 2:
            continue;
        default:
            a = "" + i;
            break;
        }
        s += a;
    }
    return s === "ab3" && i === 4;
}
