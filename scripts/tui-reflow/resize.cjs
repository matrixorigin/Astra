// Exercise the shipped binary and actual xterm reflow, not a widget rectangle
// or a text parser that cannot move existing rows on terminal resize.
const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');
const { Terminal } = require('@xterm/headless');

const binary = path.resolve(process.env.ASTRA_TEST_BINARY || '../../target/debug/astra');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

async function journey(sizes, draft = false, rapid = false, displacedCursor = false, expiredReply = false) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'astra-reflow-'));
  const terminal = new Terminal({ cols: 100, rows: 30, scrollback: 2000, allowProposedApi: true });
  const child = spawn('python3', ['-u', path.join(__dirname, 'pty_bridge.py'), binary, root], {
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const exited = once(child, 'exit');
  const trace = [];
  const started = Date.now();
  let tracingResize = false;
  let cursorReports = 0;
  let holdCursorReply = false;
  let heldCursorReply;
  const send = message => {
    if (tracingResize) snapshot('send ' + JSON.stringify(message));
    child.stdin.write(JSON.stringify(message) + '\n');
  };
  terminal.onData(input => {
    if (/\x1b\[\d+;\d+R/.test(input)) cursorReports++;
    if (tracingResize) snapshot('reply ' + JSON.stringify(input));
    // A slow terminal reply must not let a resize draw use an older geometry.
    if (rapid && /\x1b\[\d+;\d+R/.test(input)) {
      if (holdCursorReply) {
        heldCursorReply = input;
        return;
      }
      setTimeout(() => { if (child.exitCode === null) send({ input }); }, 25);
    } else {
      send({ input });
    }
  });
  let writes = Promise.resolve();
  let pendingResize = null;
  let lastOutput = Date.now();
  const output = [];
  readline.createInterface({ input: child.stdout }).on('line', line => {
    const message = JSON.parse(line);
    if (message.resize) {
      const applyResize = pendingResize;
      writes = writes.then(() => {
        // A timed-out request has already failed; its late ACK must not
        // replace that failure or interfere with cleanup/a later request.
        if (applyResize && pendingResize === applyResize) return applyResize(message.resize);
      });
      return;
    }
    const data = Buffer.from(message.data, 'base64');
    output.push(data);
    lastOutput = Date.now();
    writes = writes.then(() => new Promise(resolve => terminal.write(data, resolve))).then(() => {
      if (tracingResize) snapshot('output');
    });
  });
  const lines = () => {
    const buffer = terminal.buffer.active;
    return Array.from({ length: buffer.length }, (_, i) => buffer.getLine(i).translateToString(true));
  };
  function snapshot(label) {
    const b = terminal.buffer.active;
    trace.push({label, elapsedMs: Date.now() - started, size: [terminal.cols, terminal.rows], base: b.baseY,
      cursor: [b.cursorX, b.cursorY],
      ui: lines().map((v, row) => ({row, v})).filter(x => /DRAFT_SENTINEL|Message Astra|resize-model.*Ask/.test(x.v))});
  }
  async function resize(cols, rows, displaceCursor = false) {
    assert.equal(pendingResize, null, 'only one PTY resize may be outstanding');
    let timer;
    try {
      await new Promise((resolve, reject) => {
        timer = setTimeout(() => reject(new Error('PTY resize was not acknowledged')), 15000);
        pendingResize = async size => {
          try {
            assert.deepEqual(size, [cols, rows]);
            // Apply geometry in the same ordered stream as output. A DSR
            // must not observe the new emulator size before TIOCSWINSZ, or
            // an old output chunk after geometry from a later command.
            terminal.resize(cols, rows);
            if (displaceCursor) {
              await new Promise(done => terminal.write('\x1b[2B', done));
            }
            resolve();
          } catch (error) { reject(error); }
        };
        send({ resize: [cols, rows] });
      });
    } finally {
      clearTimeout(timer);
      pendingResize = null;
    }
  }
  async function waitFor(predicate) {
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      await delay(30);
      await writes;
      if (predicate()) return;
      if (child.exitCode !== null) break;
    }
    throw new Error('UI did not settle:\n' + lines().join('\n'));
  }
  async function settle() {
    await delay(200);
    await waitFor(() => Date.now() - lastOutput >= 200);
  }
  function check(label) {
    const all = lines();
    const visible = all.slice(terminal.buffer.active.baseY, terminal.buffer.active.baseY + terminal.rows);
    const footers = all.filter(line => line.includes('resize-model') && line.includes('Ask'));
    const prompts = all.filter(line => line.includes(draft ? 'DRAFT_SENTINEL' : 'Message Astra'));
    const context = label + '\n' + all.join('\n');
    assert.equal(footers.length, 1, 'footer leaked into screen/scrollback: ' + context);
    assert.equal(prompts.length, 1, 'composer leaked into screen/scrollback: ' + context);
    assert(visible.some(line => line.includes(draft ? 'DRAFT_SENTINEL' : 'Message Astra')), 'live composer not visible: ' + context);
    assert.equal(all.filter(line => line.includes('RESIZE_HISTORY_SENTINEL')).length, 1,
      'pre-existing history was lost or duplicated: ' + context);
    const history = all.join('').replace(/\s/g, '').match(/RESIZE_HISTORY_SENTINEL((?:history)+)/);
    assert.equal(history?.[1], 'history'.repeat(20), 'history text was damaged: ' + context);
    const tips = all.filter(line => line.includes('· Tip'));
    assert.equal(tips.length, visible.filter(line => line.includes('· Tip')).length,
      'a tip leaked into scrollback: ' + context);
    if (draft) assert.equal(tips.length, 0, 'tips should yield to a draft: ' + context);
    else if (terminal.cols >= 80) {
      assert.equal(tips.length, 1, 'the live composer should show a tip: ' + context);
      assert(tips[0].includes('Message Astra'), 'the tip left its input row: ' + context);
    }
    assert.equal(all.filter(line => line.includes('Workspace trusted')).length, 1,
      'committed startup history was lost or duplicated: ' + context);
    assert(!Buffer.concat(output).includes(Buffer.from('\x1b[3J')), 'resize purged native scrollback');
    console.log('PASS', label);
  }
  try {
    await waitFor(() => lines().some(line => /trust|Trust/.test(line)) ||
      lines().some(line => line.includes('Message Astra')));
    if (!lines().some(line => line.includes('Message Astra'))) send({ input: '\r' });
    await waitFor(() => lines().some(line => line.includes('Message Astra')));
    await settle();
    if (draft) {
      send({ input: '\x1b[200~DRAFT_SENTINEL ' + 'draft '.repeat(20) + '\x1b[201~' });
      await waitFor(() => lines().some(line => line.includes('DRAFT_SENTINEL')));
      await settle();
    }
    check('initial 100x30');
    if (expiredReply) {
      tracingResize = true;
      holdCursorReply = true;
      const before = cursorReports;
      await resize(expiredReply === 'width' ? 40 : 100, 15);
      await waitFor(() => cursorReports > before);
      const expired = heldCursorReply;
      if (expiredReply === 'width') await settle();
      await resize(100, 30);
      // Outlast the query deadline. A terminal with no response must remain
      // usable, but another indistinguishable DSR must not overlap this one.
      await settle();
      send({ input: '\x1b[200~ PENDING_INPUT\x1b[201~' });
      if (expiredReply !== 'width') await waitFor(() => lines().join('').includes('PENDING_INPUT'));
      await settle();
      assert.equal(cursorReports, before + 1, 'issued another cursor query before the expired reply was consumed');
      if (expiredReply === 'width') {
        assert.equal(lines().filter(line => line.includes('Workspace trusted')).length, 1,
          'width query timeout erased committed history:\n' + lines().join('\n'));
      } else {
        check('cursor reply deadline; paste remains responsive');
      }
      holdCursorReply = false;
      send({ input: expired });
      send({ input: '\x1b[200~ RECOVERED_INPUT\x1b[201~' });
      await waitFor(() => lines().join('').includes('RECOVERED_INPUT'));
      await resize(160, 30);
      await waitFor(() => cursorReports > before + 1);
      await settle();
      assert(lines().join('').includes('PENDING_INPUT'), 'input was lost during cursor reply quarantine');
      check('late cursor reply quarantined; fresh resize recovered');
      return;
    }
    tracingResize = rapid;
    for (const [index, [cols, rows]] of sizes.entries()) {
      const reportsBeforeResize = cursorReports;
      if (rapid && index === 0) holdCursorReply = true;
      // Apply the final cursor displacement before ACK-following output can
      // issue a query, rather than depending on the JavaScript continuation.
      await resize(cols, rows, displacedCursor && index === sizes.length - 1);
      if (rapid) snapshot('resize');
      if (rapid && index === 1) {
        holdCursorReply = false;
        const input = heldCursorReply;
        assert(input, 'first resize must have a cursor reply in flight');
        setTimeout(() => { if (child.exitCode === null) send({ input }); }, 25);
      }
      // Establish a real query in flight before racing subsequent resizes;
      // do not rely on the Rust process being scheduled within ten ms in CI.
      if (!rapid || index === 0) await waitFor(() => cursorReports > reportsBeforeResize);
      if (rapid) {
        await delay(10);
      } else {
        await settle();
        check(`${cols}x${rows}`);
      }
    }
    if (rapid) {
      await settle();
      check(displacedCursor ? 'rapid resize with cursor displacement' : 'rapid resize round trips');
    }
  } catch (error) {
    if (rapid) console.error(JSON.stringify(trace, null, 2));
    throw error;
  } finally {
    if (child.exitCode === null) send({ stop: true });
    await exited;
    terminal.dispose();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

(async () => {
  assert(fs.existsSync(binary), 'build astra or set ASTRA_TEST_BINARY');
  await journey([[40, 30], [160, 30], [100, 30], [40, 30], [160, 30]]);
  await journey([[100, 15], [100, 30], [100, 10], [100, 35], [40, 15], [160, 30]]);
  await journey([[40, 30], [160, 30], [40, 15], [100, 30]], true);
  const rapidSizes = Array.from({ length: 3 }, () => [[40, 30], [160, 30], [100, 15], [100, 30]]).flat();
  await journey(rapidSizes, false, true);
  await journey(rapidSizes, true, true);
  await journey(rapidSizes, true, true, true);
  await journey([], true, true, false, true);
  await journey([], true, true, false, 'width');
})().catch(error => { console.error(error); process.exitCode = 1; });
