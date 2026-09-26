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

async function journey(sizes, draft = false, rapid = false, displacedCursor = false) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'astra-reflow-'));
  const terminal = new Terminal({ cols: 100, rows: 30, scrollback: 2000, allowProposedApi: true });
  const child = spawn('python3', ['-u', path.join(__dirname, 'pty_bridge.py'), binary, root], {
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const exited = once(child, 'exit');
  const trace = [];
  let tracingResize = false;
  let cursorReports = 0;
  let holdCursorReply = false;
  let heldCursorReply;
  const send = message => child.stdin.write(JSON.stringify(message) + '\n');
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
  let lastOutput = Date.now();
  const output = [];
  readline.createInterface({ input: child.stdout }).on('line', line => {
    const data = Buffer.from(JSON.parse(line).data, 'base64');
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
    trace.push({label, size: [terminal.cols, terminal.rows], base: b.baseY,
      cursor: [b.cursorX, b.cursorY],
      ui: lines().map((v, row) => ({row, v})).filter(x => /DRAFT_SENTINEL|Message Astra|resize-model.*Ask/.test(x.v))});
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
    assert.equal((all.join('').match(/history/g) || []).length, 20, 'history text was damaged: ' + context);
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
    tracingResize = rapid;
    for (const [index, [cols, rows]] of sizes.entries()) {
      const reportsBeforeResize = cursorReports;
      if (rapid && index === 0) holdCursorReply = true;
      terminal.resize(cols, rows);
      if (displacedCursor && index === sizes.length - 1) {
        // Reflow can move the cursor below cells it retained. Reproduce that
        // geometry before the final cursor query instead of relying on CI timing.
        await new Promise(resolve => terminal.write('\x1b[2B', resolve));
      }
      send({ resize: [cols, rows] });
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
})().catch(error => { console.error(error); process.exitCode = 1; });
