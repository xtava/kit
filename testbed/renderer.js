'use strict';

const params = new URLSearchParams(location.search);
const apiPort = params.get('api');
const api = (path) => `http://127.0.0.1:${apiPort}${path}`;

window.testbed = {
  version: '1.0.0',
  counter: 0,
  items: [],
  lastSave: null,
  ticking: false,
};

document.getElementById('api-info').textContent = `api: 127.0.0.1:${apiPort}`;
console.log('[testbed] renderer booted', { version: window.testbed.version, api: apiPort });

if (params.get('bootError')) {
  setTimeout(() => {
    throw new Error('intentional boot error');
  }, 0);
}

const on = (id, handler) => document.getElementById(id).addEventListener('click', handler);

// --- console ---------------------------------------------------------------
on('log-info', () => console.log('[testbed] info message', { counter: window.testbed.counter }));
on('log-warn', () => console.warn('[testbed] warning message'));
on('log-error', () => console.error('[testbed] error message', { code: 1337 }));
on('throw-exception', () => {
  setTimeout(() => {
    throw new Error('intentional uncaught exception');
  }, 0);
});
on('unhandled-rejection', () => {
  Promise.reject(new Error('intentional unhandled rejection'));
});
on('burst-logs', () => {
  for (let index = 1; index <= 6; index += 1) {
    setTimeout(() => console.log(`[testbed] burst ${index}/6`), index * 250);
  }
});

// --- network ---------------------------------------------------------------
const hit = (path) => fetch(api(path)).then(
  (response) => console.log(`[testbed] ${path} → ${response.status}`),
  (error) => console.error(`[testbed] ${path} failed`, error.message)
);
on('fetch-ok', () => hit('/api/ok'));
on('fetch-500', () => hit('/api/fail'));
on('fetch-404', () => hit('/api/notfound'));
on('fetch-slow', () => hit('/api/slow?ms=1500'));
on('fetch-flaky', () => hit('/api/flaky'));

// --- websocket --------------------------------------------------------------
let ws = null;
let pings = 0;
const wsStatus = document.getElementById('ws-status');
on('ws-send', () => {
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    ws = new WebSocket(`ws://127.0.0.1:${apiPort}/ws`);
    ws.onopen = () => {
      wsStatus.textContent = 'connected';
      ws.send(`ping #${(pings += 1)}`);
    };
    ws.onmessage = (message) => console.log('[testbed] ws received:', message.data);
    ws.onclose = () => {
      wsStatus.textContent = 'disconnected';
    };
    return;
  }
  ws.send(`ping #${(pings += 1)}`);
});

// --- async save ---------------------------------------------------------------
const saveStatus = document.getElementById('save-status');
const toast = (text, isError) => {
  const node = document.createElement('div');
  node.setAttribute('role', 'alert');
  node.textContent = text;
  if (isError) node.className = 'error';
  document.getElementById('toasts').append(node);
  setTimeout(() => node.remove(), 2500);
};
const save = async (path, button) => {
  button.disabled = true;
  saveStatus.textContent = 'saving…';
  try {
    const response = await fetch(api(path), { method: 'POST', body: 'settings=1' });
    if (!response.ok) throw new Error(`save failed with ${response.status}`);
    window.testbed.lastSave = Date.now();
    saveStatus.textContent = 'saved';
    toast('Saved');
  } catch (error) {
    console.error('[testbed] save failed:', error.message);
    saveStatus.textContent = 'save failed';
    toast('Save failed', true);
  } finally {
    button.disabled = false;
  }
};
on('save-settings', (event) => save('/api/save', event.target));
on('save-failing', (event) => save('/api/fail', event.target));

// --- state ---------------------------------------------------------------
const counterValue = document.getElementById('counter-value');
const bump = () => {
  window.testbed.counter += 1;
  counterValue.textContent = String(window.testbed.counter);
};
on('increment', bump);

let ticker = null;
on('ticker-toggle', (event) => {
  if (ticker) {
    clearInterval(ticker);
    ticker = null;
    window.testbed.ticking = false;
    event.target.textContent = 'start ticker';
  } else {
    ticker = setInterval(bump, 1000);
    window.testbed.ticking = true;
    event.target.textContent = 'stop ticker';
  }
});

const cart = document.getElementById('cart');
on('add-item', () => {
  const name = `item ${window.testbed.items.length + 1}`;
  window.testbed.items.push(name);
  const item = document.createElement('li');
  item.className = 'cart-item';
  item.textContent = name;
  cart.append(item);
});
on('remove-item', () => {
  window.testbed.items.pop();
  cart.lastElementChild?.remove();
});

// --- form ---------------------------------------------------------------
document.getElementById('signup').addEventListener('submit', (event) => {
  event.preventDefault();
  const name = document.getElementById('name').value.trim();
  if (!name) {
    console.error('[testbed] form validation failed: name is required');
    toast('Name is required', true);
    return;
  }
  const flavor = document.getElementById('flavor').value;
  const subscribed = document.getElementById('subscribe').checked;
  console.log('[testbed] form submitted', { name, flavor, subscribed });
  document.getElementById('form-result').textContent =
    `submitted: ${name} / ${flavor} / ${subscribed ? 'subscribed' : 'not subscribed'}`;
});

// --- navigation ---------------------------------------------------------------
document.getElementById('goto-page2').href = `page2.html?api=${apiPort}`;
