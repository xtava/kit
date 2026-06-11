// bundle.js — "built" from src/cart.js by prepending this 3-line banner; the map encodes the shift.
// Hand-maintained: editing src/cart.js means re-pasting it below and keeping the map's run in sync.
'use strict';
// testbed cart — the "original" source behind bundle.js (line-shifted by its 3-line banner)
let items = [];

function addItem(name) {
  items.push(name);
  return items.length;
}

function totalItems() {
  return items.length;
}

function brokenTotal() {
  return items.reduce((sum, item) => sum + item.price.amount, 0);
}

window.testbed.cart = { addItem, totalItems, brokenTotal, items };
//# sourceMappingURL=bundle.js.map
