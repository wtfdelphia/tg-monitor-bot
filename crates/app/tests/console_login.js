const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const script = fs.readFileSync(0, 'utf8');
const nonce = 'A'.repeat(43);
const initialTime = Date.parse('2026-10-12T09:00:00Z');

function element() {
  return {
    disabled: false,
    hidden: true,
    style: {},
    href: '',
    textContent: '',
    events: new Map(),
    addEventListener(name, handler) { this.events.set(name, handler); },
    removeAttribute(name) { delete this[name]; },
    async click() {
      assert.equal(this.disabled, false);
      await this.events.get('click')();
    },
  };
}

function browser() {
  let now = initialTime;
  let timerId = 0;
  const timers = new Map();
  const calls = [];
  const responses = [];
  const nodes = new Map(['#qr-start', '#qr', '#qr-code', '#qr-link', '#qr-state'].map(name => [name, element()]));
  nodes.get('#qr').style.display = 'none';
  const window = { location: { href: '' } };
  vm.runInNewContext(script, {
    window,
    URL,
    Date: class extends Date { static now() { return now; } },
    AbortSignal: { timeout: milliseconds => ({ timeout: milliseconds }) },
    document: { querySelector: selector => nodes.get(selector) },
    fetch: async (url, options) => {
      calls.push({ url, options });
      const queued = responses.shift();
      assert.ok(queued, `Unexpected request: ${url}`);
      const response = await queued;
      if (response.error) throw response.error;
      return {
        status: response.status,
        ok: response.status >= 200 && response.status < 300,
        json: async () => {
          if (response.jsonError) throw response.jsonError;
          return response.body;
        },
      };
    },
    setTimeout: (handler, delay) => {
      timerId += 1;
      timers.set(timerId, { handler, delay });
      return timerId;
    },
    clearTimeout: identifier => timers.delete(identifier),
  }, { timeout: 1000 });
  return {
    nodes, calls, responses, timers, window,
    advance(milliseconds) { now += milliseconds; },
    async tick() {
      assert.equal(timers.size, 1);
      const [identifier, timer] = timers.entries().next().value;
      timers.delete(identifier);
      now += timer.delay;
      await timer.handler();
    },
  };
}

function challenge(overrides = {}) {
  return {
    status: 201,
    body: {
      nonce,
      confirm_code: 'ABCD',
      expires_at: new Date(initialTime + 120000).toISOString(),
      deep_link: `https://t.me/test_control_bot?start=login_${nonce}`,
      ...overrides,
    },
  };
}

function status(state) { return { status: 200, body: { state } }; }

function assertStopped(state, message) {
  assert.equal(state.nodes.get('#qr-start').disabled, false);
  assert.equal(state.nodes.get('#qr-link').hidden, true);
  assert.equal(state.timers.size, 0);
  assert.equal(state.window.location.href, '');
  assert.match(state.nodes.get('#qr-state').textContent, message);
}

async function run() {
  const success = browser();
  success.responses.push(challenge());
  await success.nodes.get('#qr-start').click();
  assert.equal(success.nodes.get('#qr').style.display, 'block');
  assert.equal(success.nodes.get('#qr-start').disabled, true);
  assert.equal(success.nodes.get('#qr-code').textContent, '确认码：ABCD');
  assert.equal(success.nodes.get('#qr-link').hidden, false);
  assert.equal(success.nodes.get('#qr-link').href, challenge().body.deep_link);
  assert.match(success.nodes.get('#qr-state').textContent, /比对确认码/);
  for (const pending of ['pending', 'awaiting_confirm']) {
    success.responses.push(status(pending));
    await success.tick();
    assert.equal(success.timers.size, 1);
    assert.equal(success.nodes.get('#qr-start').disabled, true);
    assert.match(success.nodes.get('#qr-state').textContent, pending === 'pending' ? /等待 Bot/ : /仅在一致时确认/);
  }
  success.responses.push(status('confirmed'));
  await success.tick();
  assert.equal(success.timers.size, 0);
  assert.equal(success.window.location.href, '/console/identities');
  assert.equal(success.nodes.get('#qr-start').disabled, true);
  assert.equal(success.calls[0].options.method, 'POST');
  assert.equal(success.calls[0].options.headers['content-type'], 'application/json');
  for (const [index, request] of success.calls.entries()) {
    assert.equal(request.options.credentials, 'same-origin');
    assert.equal(request.options.signal.timeout, 10000);
    assert.equal(request.options.body, undefined);
    assert.equal(request.url.includes('tenant'), false);
    if (index > 0) assert.equal(request.url, `/auth/qr/challenge/${nonce}`);
  }

  for (const response of [
    { status: 500 },
    { error: new Error('network disconnected') },
    { error: new Error('TimeoutError') },
    { status: 201, jsonError: new Error('invalid JSON') },
  ]) {
    const failed = browser();
    failed.responses.push(response);
    await failed.nodes.get('#qr-start').click();
    assert.equal(failed.nodes.get('#qr').style.display, 'block');
    assertStopped(failed, /无法创建登录挑战/);
    failed.responses.push(challenge());
    await failed.nodes.get('#qr-start').click();
    assert.equal(failed.timers.size, 1);
    assert.equal(failed.nodes.get('#qr-link').hidden, false);
  }

  for (const terminal of ['expired', 'rejected']) {
    const stopped = browser();
    stopped.responses.push(challenge(), status(terminal));
    await stopped.nodes.get('#qr-start').click();
    await stopped.tick();
    assertStopped(stopped, terminal === 'expired' ? /已过期/ : /已被拒绝/);
    stopped.responses.push({ status: 500 });
    await stopped.nodes.get('#qr-start').click();
    assert.equal(stopped.nodes.get('#qr-code').textContent, '');
    assert.equal(stopped.nodes.get('#qr-link').textContent, '');
    assert.equal(stopped.nodes.get('#qr-link').href, undefined);
  }

  for (const response of [
    { status: 404 },
    { status: 500 },
    { error: new Error('network disconnected') },
    { error: new Error('TimeoutError') },
    { status: 200, jsonError: new Error('invalid JSON') },
    status('unexpected'),
  ]) {
    const failed = browser();
    failed.responses.push(challenge(), response);
    await failed.nodes.get('#qr-start').click();
    await failed.tick();
    assertStopped(failed, /轮询失败/);
  }

  for (const phase of ['create', 'poll']) {
    const notInvited = browser();
    if (phase === 'poll') notInvited.responses.push(challenge());
    notInvited.responses.push({ status: 403, body: { error: { code: 'NOT_INVITED' } } });
    await notInvited.nodes.get('#qr-start').click();
    if (phase === 'poll') await notInvited.tick();
    assertStopped(notInvited, /尚未被邀请.*联系工作区管理员/);
  }

  for (const invalid of [
    { deep_link: 'javascript:alert(1)' },
    { deep_link: `https://example.com/test?start=login_${nonce}` },
    { deep_link: `https://user:password@t.me/test?start=login_${nonce}` },
    { deep_link: 'https://t.me/test?start=login_wrong' },
    { expires_at: 'invalid' },
    { expires_at: new Date(initialTime - 1).toISOString() },
    { confirm_code: '<img src=x>' },
    { nonce: '../other' },
  ]) {
    const failed = browser();
    failed.responses.push(challenge(invalid));
    await failed.nodes.get('#qr-start').click();
    assertStopped(failed, /无法创建登录挑战/);
    assert.equal(failed.nodes.get('#qr-link').href, undefined);
  }

  for (const body of [null, {}, { nonce }]) {
    const malformed = browser();
    malformed.responses.push({ status: 201, body });
    await malformed.nodes.get('#qr-start').click();
    assertStopped(malformed, /无法创建登录挑战/);
  }

  const expired = browser();
  expired.responses.push(challenge());
  await expired.nodes.get('#qr-start').click();
  expired.advance(120000);
  await expired.tick();
  assertStopped(expired, /已过期/);
  assert.equal(expired.calls.length, 1);

  const creation = browser();
  let resolveCreation;
  creation.responses.push(new Promise(resolve => { resolveCreation = resolve; }));
  const creating = creation.nodes.get('#qr-start').click();
  await new Promise(resolve => setImmediate(resolve));
  await creation.nodes.get('#qr-start').events.get('click')();
  assert.equal(creation.calls.length, 1);
  assert.equal(creation.timers.size, 0);
  resolveCreation(challenge());
  await creating;
  assert.equal(creation.timers.size, 1);

  const serial = browser();
  serial.responses.push(challenge());
  await serial.nodes.get('#qr-start').click();
  let resolvePoll;
  serial.responses.push(new Promise(resolve => { resolvePoll = resolve; }));
  const waiting = serial.tick();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(serial.calls.length, 2);
  assert.equal(serial.timers.size, 0);
  await serial.nodes.get('#qr-start').events.get('click')();
  assert.equal(serial.calls.length, 2);
  resolvePoll(status('pending'));
  await waiting;
  assert.equal(serial.timers.size, 1);
}

run().catch(error => { console.error(error); process.exitCode = 1; });
