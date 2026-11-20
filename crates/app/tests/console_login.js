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
    src: '',
    style: {},
    href: '',
    textContent: '',
    events: new Map(),
    addEventListener(name, handler) { this.events.set(name, handler); },
    removeAttribute(name) { if (name === 'src') this.src = ''; else delete this[name]; },
    async click() {
      assert.equal(this.disabled, false);
      await this.events.get('click')();
    },
  };
}

function browser(source = script) {
  let now = initialTime;
  let timerId = 0;
  const timers = new Map();
  const calls = [];
  const responses = [];
  const nodes = new Map(['#qr-start', '#qr', '#qr-code', '#qr-image', '#qr-link', '#qr-state'].map(name => [name, element()]));
  nodes.get('#qr').style.display = 'none';
  const window = { location: { href: '' }, events: new Map(), addEventListener(name, handler) { this.events.set(name, handler); } };
  vm.runInNewContext(source, {
    window,
    URL,
    Date: class extends Date { static now() { return now; } },
    AbortController,
    AbortSignal,
    DOMParser: class {
      parseFromString(value) {
        const invalid = value.includes('<script') || value.includes('href=');
        const attributes = names => names.map(name => ({ name, value: name === 'fill' ? '#000' : '' }));
        const nodes = invalid
          ? [{ localName: 'script', namespaceURI: 'http://www.w3.org/2000/svg', attributes: [], children: [] }]
          : [
            { localName: 'rect', namespaceURI: 'http://www.w3.org/2000/svg', attributes: attributes(['x', 'y', 'width', 'height', 'fill']), children: [] },
            { localName: 'path', namespaceURI: 'http://www.w3.org/2000/svg', attributes: attributes(['fill', 'd']), children: [] },
          ];
        return {
          querySelector: () => invalid ? {} : null,
          documentElement: {
            localName: 'svg',
            namespaceURI: 'http://www.w3.org/2000/svg',
            outerHTML: value,
            attributes: attributes(['xmlns', 'version', 'width', 'height', 'viewBox', 'shape-rendering']),
            querySelectorAll: () => nodes,
          },
        };
      }
    },
    encodeURIComponent,
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
    pollTimers() { return [...timers].filter(([, timer]) => timer.delay <= 2000); },
    advance(milliseconds) { now += milliseconds; },
    async tick() {
      const [identifier, timer] = [...timers].sort((first, second) => first[1].delay - second[1].delay)[0];
      timers.delete(identifier);
      now += timer.delay;
      await timer.handler();
    },
    async expire() {
      const [identifier, timer] = [...timers].sort((first, second) => second[1].delay - first[1].delay)[0];
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
      qr_svg: '<svg xmlns="http://www.w3.org/2000/svg" version="1.1" width="256" height="256" viewBox="0 0 256 256" shape-rendering="crispEdges"><rect x="0" y="0" width="256" height="256" fill="#fff"/><path fill="#000" d="M1 1"/></svg>',
      ...overrides,
    },
  };
}

function status(state) { return { status: 200, body: { state } }; }

function assertStopped(state, message) {
  assert.equal(state.nodes.get('#qr-start').disabled, false);
  assert.equal(state.nodes.get('#qr-link').hidden, true);
  assert.equal(state.nodes.get('#qr-image').hidden, true);
  assert.equal(state.nodes.get('#qr-image').src, '');
  assert.equal(state.nodes.get('#qr-code').textContent, '');
  assert.equal(state.nodes.get('#qr-link').href, undefined);
  assert.equal(state.nodes.get('#qr-link').textContent, '');
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
  assert.equal(success.nodes.get('#qr-image').hidden, false);
  assert.match(success.nodes.get('#qr-image').src, /^data:image\/svg\+xml;charset=utf-8,/);
  assert.equal(success.nodes.get('#qr-link').hidden, false);
  assert.equal(success.nodes.get('#qr-link').href, challenge().body.deep_link);
  assert.match(success.nodes.get('#qr-state').textContent, /比对确认码/);
  for (const pending of ['pending', 'awaiting_confirm']) {
    success.responses.push(status(pending));
    await success.tick();
    assert.equal(success.pollTimers().length, 1);
    assert.equal(success.nodes.get('#qr-start').disabled, true);
    assert.match(success.nodes.get('#qr-state').textContent, pending === 'pending' ? /等待 Bot/ : /仅在一致时确认/);
  }
  success.responses.push(status('confirmed'));
  await success.tick();
  assert.equal(success.timers.size, 0);
  assert.equal(success.window.location.href, '/console/identities');
  assert.equal(success.nodes.get('#qr-image').hidden, true);
  assert.equal(success.nodes.get('#qr-image').src, '');
  assert.equal(success.nodes.get('#qr-start').disabled, true);
  assert.equal(success.calls[0].options.method, 'POST');
  assert.equal(success.calls[0].options.headers['content-type'], 'application/json');
  for (const [index, request] of success.calls.entries()) {
    assert.equal(request.options.credentials, 'same-origin');
    assert.equal(request.options.cache, 'no-store');
    assert.equal(typeof request.options.signal.aborted, 'boolean');
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
    assert.equal(failed.pollTimers().length, 1);
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
    { qr_svg: null },
    { qr_svg: 'x'.repeat(524289) },
    { qr_svg: '<script>alert(1)</script>' },
    { qr_svg: '<svg xmlns="http://www.w3.org/2000/svg"><image href="https://evil.invalid/x"/></svg>' },
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

  const leaving = browser();
  leaving.responses.push(challenge());
  await leaving.nodes.get('#qr-start').click();
  assert.equal(leaving.nodes.get('#qr-image').hidden, false);
  leaving.window.events.get('pagehide')();
  assert.equal(leaving.nodes.get('#qr-image').hidden, true);
  assert.equal(leaving.nodes.get('#qr-image').src, '');
  assert.equal(leaving.nodes.get('#qr-link').href, undefined);
  assert.equal(leaving.timers.size, 0);

  const imageError = browser();
  imageError.responses.push(challenge());
  await imageError.nodes.get('#qr-start').click();
  imageError.nodes.get('#qr-image').events.get('error')();
  assert.equal(imageError.nodes.get('#qr-image').hidden, true);
  assert.equal(imageError.nodes.get('#qr-link').hidden, false);
  assert.match(imageError.nodes.get('#qr-state').textContent, /可使用下方 Telegram 链接/);

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
  assert.equal(creation.pollTimers().length, 1);

  const serial = browser();
  serial.responses.push(challenge());
  await serial.nodes.get('#qr-start').click();
  let resolvePoll;
  serial.responses.push(new Promise(resolve => { resolvePoll = resolve; }));
  const waiting = serial.tick();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(serial.calls.length, 2);
  assert.equal(serial.pollTimers().length, 0);
  await serial.nodes.get('#qr-start').events.get('click')();
  assert.equal(serial.calls.length, 2);
  resolvePoll(status('pending'));
  await waiting;
  assert.equal(serial.pollTimers().length, 1);

  for (const phase of ['create', 'poll']) {
    const late = browser();
    let resolveLate;
    if (phase === 'poll') {
      late.responses.push(challenge());
      await late.nodes.get('#qr-start').click();
    }
    late.responses.push(new Promise(resolve => { resolveLate = resolve; }));
    const pending = phase === 'create' ? late.nodes.get('#qr-start').click() : late.tick();
    await new Promise(resolve => setImmediate(resolve));
    late.window.events.get('pagehide')();
    assert.equal(late.calls.at(-1).options.signal.aborted, true);
    resolveLate(phase === 'create' ? challenge() : status('confirmed'));
    await pending;
    assertStopped(late, /正在创建|手机相机/);
  }

  const inflight = browser();
  inflight.responses.push(challenge());
  await inflight.nodes.get('#qr-start').click();
  let resolveExpired;
  inflight.responses.push(new Promise(resolve => { resolveExpired = resolve; }));
  const expiringPoll = inflight.tick();
  await new Promise(resolve => setImmediate(resolve));
  await inflight.expire();
  assertStopped(inflight, /已过期/);
  resolveExpired(status('confirmed'));
  await expiringPoll;
  assertStopped(inflight, /已过期/);

  for (const [original, replacement, check] of [
    ["clearImage();code.textContent='';", "code.textContent='';", async state => {
      state.responses.push(challenge(), status('expired'));
      await state.nodes.get('#qr-start').click();
      await state.tick();
      assertStopped(state, /已过期/);
    }],
    ["if(current!==generation)return;", '', async state => {
      let resolveLate;
      state.responses.push(new Promise(resolve => { resolveLate = resolve; }));
      const creating = state.nodes.get('#qr-start').click();
      await new Promise(resolve => setImmediate(resolve));
      state.window.events.get('pagehide')();
      resolveLate(challenge());
      await creating;
      assert.equal(state.nodes.get('#qr-image').hidden, true);
    }],
  ]) {
    assert.ok(script.includes(original), '反证必须命中页面实现');
    await assert.rejects(check(browser(script.split(original).join(replacement))));
  }
}

run().catch(error => { console.error(error); process.exitCode = 1; });
