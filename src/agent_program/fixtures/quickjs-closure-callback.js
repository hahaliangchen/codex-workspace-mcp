// Adapted from QuickJS tests/test_closure.js, test_closure1.
// Copyright (c) 2017-2021 Fabrice Bellard and Charlie Gordon.
// See quickjs-LICENSE.txt for the MIT license.
function main() {
    function f2() {
        var val = 1;
        function set(a) { val = a; }
        function get(a) { return val; }
        return { "set": set, "get": get };
    }
    var obj = f2();
    obj.set(10);
    return [0].map(function (x) { return obj.get(); })[0];
}
