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
