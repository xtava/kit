'use strict';

const { app, BrowserWindow } = require('electron');
const http = require('node:http');
const crypto = require('node:crypto');

// kit cdp launch-electron exports the renderer debug port under --cdp-env; the switch must be
// appended before app ready or Chromium ignores it.
if (process.env.TESTBED_CDP_PORT) {
  app.commandLine.appendSwitch('remote-debugging-port', process.env.TESTBED_CDP_PORT);
}

let flakyAttempts = 0;

function json(res, status, body) {
  res.writeHead(status, {
    'content-type': 'application/json',
    'access-control-allow-origin': '*',
    'access-control-allow-headers': '*',
  });
  res.end(JSON.stringify(body));
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://localhost');
  if (req.method === 'OPTIONS') {
    res.writeHead(204, {
      'access-control-allow-origin': '*',
      'access-control-allow-methods': 'GET, POST, OPTIONS',
      'access-control-allow-headers': '*',
    });
    return res.end();
  }
  switch (url.pathname) {
    case '/api/ok':
      return json(res, 200, { ok: true });
    case '/api/fail':
      return json(res, 500, { error: 'intentional server failure' });
    case '/api/notfound':
      return json(res, 404, { error: 'no such thing' });
    case '/api/slow': {
      const waitMs = Number(url.searchParams.get('ms')) || 1200;
      return void setTimeout(() => json(res, 200, { ok: true, waitedMs: waitMs }), waitMs);
    }
    case '/api/flaky':
      flakyAttempts += 1;
      return flakyAttempts % 2 === 1
        ? json(res, 200, { ok: true, attempt: flakyAttempts })
        : json(res, 500, { error: 'flaky failure', attempt: flakyAttempts });
    case '/api/save': {
      let body = '';
      req.on('data', (chunk) => {
        body += chunk;
      });
      req.on('end', () =>
        setTimeout(() => json(res, 200, { saved: true, receivedBytes: body.length }), 250)
      );
      return;
    }
    default:
      return json(res, 404, { error: `unknown route ${url.pathname}` });
  }
});

// Minimal RFC 6455 echo on /ws: enough for single unfragmented text frames, which is all the
// renderer sends. Ping is answered with pong; close closes.
server.on('upgrade', (req, socket) => {
  if (new URL(req.url, 'http://localhost').pathname !== '/ws') return socket.destroy();
  const accept = crypto
    .createHash('sha1')
    .update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n' +
      'Upgrade: websocket\r\nConnection: Upgrade\r\n' +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`
  );
  socket.on('data', (frame) => {
    const opcode = frame[0] & 0x0f;
    let length = frame[1] & 0x7f;
    let offset = 2;
    if (length === 126) {
      length = frame.readUInt16BE(2);
      offset = 4;
    }
    const mask = frame.subarray(offset, offset + 4);
    const payload = Buffer.from(frame.subarray(offset + 4, offset + 4 + length));
    for (let index = 0; index < payload.length; index += 1) payload[index] ^= mask[index % 4];
    if (opcode === 0x8) return socket.end();
    if (opcode === 0x9) return socket.write(Buffer.concat([Buffer.from([0x8a, 0]), Buffer.alloc(0)]));
    const reply = Buffer.from(`echo: ${payload.toString()}`);
    socket.write(Buffer.concat([Buffer.from([0x81, reply.length]), reply]));
  });
  socket.on('error', () => socket.destroy());
});

app.whenReady().then(async () => {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const apiPort = server.address().port;
  console.log(`[testbed] api listening on 127.0.0.1:${apiPort}`);
  const window = new BrowserWindow({ width: 1100, height: 960 });
  const query = { api: String(apiPort) };
  if (process.env.TESTBED_BOOT_ERROR) query.bootError = '1';
  window.loadFile('index.html', { query });
});

app.on('window-all-closed', () => app.quit());
