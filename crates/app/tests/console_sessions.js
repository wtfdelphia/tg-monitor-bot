const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const script = fs.readFileSync(0, 'utf8');
const currentReference = 'A'.repeat(43);
const remoteReference = 'B'.repeat(43);

function element() {
  return {
    disabled: false,
    hidden: true,
    style: {},
    dataset: {},
    textContent: '',
    innerHTML: '',
    events: new Map(),
    addEventListener(name, handler) { this.events.set(name, handler); },
    insertAdjacentHTML(position, html) { this.innerHTML = html + this.innerHTML; },
    async click() {
      assert.equal(this.disabled, false);
      assert.ok(this.events.has('click'));
      await this.events.get('click')({ currentTarget: this });
    },
  };
}

async function browser(role = 'VIEWER', authenticated = true) {
  const nodes = new Map(['#content', '#title', '#me', '#sessions-open', '#logout', '#sessions-panel'].map(name => [name, element()]));
  nodes.get('#sessions-open').disabled = true;
  nodes.get('#logout').disabled = true;
  const calls = [];
  const responses = [];
  const sessionButtons = [];
  const panel = nodes.get('#sessions-panel');
  let panelHtml = '';
  Object.defineProperty(panel, 'innerHTML', {
    get: () => panelHtml,
    set: html => {
      panelHtml = html;
      sessionButtons.length = 0;
      for (const match of html.matchAll(/data-session-revoke="([^"]+)" data-session-current="([^"]+)"/g)) {
        const button = element();
        button.dataset = { sessionRevoke: match[1], sessionCurrent: match[2] };
        sessionButtons.push(button);
      }
      nodes.set('#sessions-refresh', element());
      nodes.set('#sessions-close', element());
    },
  });
  panel.insertAdjacentHTML = (position, html) => { panelHtml = html + panelHtml; };
  const window = {
    location: { pathname: '/console/rules/new', href: '' },
    confirm: () => true,
  };
  const context = {
    window,
    document: {
      cookie: 'other=unused; tgm_csrf=csrf-value',
      querySelector: selector => nodes.get(selector) || null,
      querySelectorAll: selector => selector === '[data-session-revoke]' ? sessionButtons : [],
    },
    fetch: async (url, options) => {
      calls.push({ url, options });
      const response = url === '/auth/me'
        ? { status: authenticated ? 200 : 401, body: { role, user_id: 7015 } }
        : responses.shift();
      assert.ok(response, `Unexpected request: ${url}`);
      return {
        status: response.status,
        ok: response.status >= 200 && response.status < 300,
        headers: { get: () => response.status === 204 ? '' : 'application/json' },
        json: async () => response.body,
        text: async () => '',
      };
    },
  };
  vm.runInNewContext(script, context, { timeout: 1000 });
  await new Promise(resolve => setImmediate(resolve));
  return { nodes, panel, calls, responses, sessionButtons, window };
}

function activeSessions() {
  return {
    status: 200,
    body: { items: [
      { id: currentReference, is_current: true, ip: '203.0.113.1', user_agent: '<img src=x onerror=alert(1)>', login_method: 'oidc', created_at: '2026-10-12T09:00:00Z', expires_at: '2026-11-11T09:00:00Z' },
      { id: remoteReference, is_current: false, ip: null, user_agent: 'Remote browser', login_method: 'bot_qr', created_at: '2026-10-11T09:00:00Z', expires_at: '2026-11-10T09:00:00Z' },
      { id: 'C'.repeat(43), is_current: false, ip: null, user_agent: null, login_method: 'unknown', created_at: '2026-10-10T09:00:00Z', expires_at: '2026-11-09T09:00:00Z' },
    ] },
  };
}

async function run() {
  for (const role of ['VIEWER', 'MEMBER']) {
    const state = await browser(role);
    assert.equal(state.nodes.get('#sessions-open').disabled, false);
    assert.equal(state.nodes.get('#logout').disabled, false);
    state.responses.push(activeSessions());
    await state.nodes.get('#sessions-open').click();
    assert.equal(state.panel.hidden, false);
    assert.ok(state.panel.innerHTML.includes('Telegram OIDC'));
    assert.ok(state.panel.innerHTML.includes('控制 Bot 扫码'));
    assert.ok(state.panel.innerHTML.includes('未知（历史会话）'));
    assert.ok(state.panel.innerHTML.includes('&lt;img src=x onerror=alert(1)&gt;'));
    assert.ok(!state.panel.innerHTML.includes('<img'));
    const remote = state.sessionButtons.find(button => button.dataset.sessionRevoke === remoteReference);
    state.window.confirm = () => false;
    const previousCalls = state.calls.length;
    await remote.click();
    assert.equal(state.calls.length, previousCalls);
    state.window.confirm = () => true;
    state.responses.push({ status: 403, body: { error: { message: '<csrf rejected>' } } });
    await remote.click();
    assert.ok(state.panel.innerHTML.includes('&lt;csrf rejected&gt;'));
    assert.equal(remote.disabled, false);
    assert.equal(state.window.location.href, '');
    state.responses.push({ status: 204 }, { status: 200, body: { items: [activeSessions().body.items[0]] } });
    await remote.click();
    const deletes = state.calls.filter(call => call.options.method === 'DELETE');
    assert.equal(deletes.length, 2);
    for (const request of deletes) {
      assert.equal(request.url, `/auth/sessions/${remoteReference}`);
      assert.equal(request.options.headers['x-csrf-token'], 'csrf-value');
      assert.equal(request.options.credentials, 'same-origin');
      assert.equal(request.options.body, undefined);
    }
    assert.equal(state.window.location.href, '');
    assert.equal(state.sessionButtons.length, 1);
    state.responses.push({ status: 204 });
    await state.sessionButtons[0].click();
    assert.equal(state.window.location.href, '/login');
    assert.equal(state.calls.at(-1).url, `/auth/sessions/${currentReference}`);
  }

  const logout = await browser();
  logout.responses.push({ status: 500, body: { error: { message: 'unavailable' } } });
  await logout.nodes.get('#logout').click();
  assert.equal(logout.window.location.href, '');
  assert.equal(logout.nodes.get('#logout').disabled, false);
  assert.ok(logout.nodes.get('#content').innerHTML.includes('unavailable'));
  logout.responses.push({ status: 204 });
  await logout.nodes.get('#logout').click();
  assert.equal(logout.window.location.href, '/login');
  assert.equal(logout.calls.at(-1).url, '/auth/logout');
  assert.equal(logout.calls.at(-1).options.method, 'POST');
  assert.equal(logout.calls.at(-1).options.headers['x-csrf-token'], 'csrf-value');

  const empty = await browser();
  empty.responses.push({ status: 200, body: { items: [] } });
  await empty.nodes.get('#sessions-open').click();
  assert.ok(empty.panel.innerHTML.includes('没有活跃会话'));
  empty.responses.push(activeSessions());
  await empty.nodes.get('#sessions-refresh').click();
  assert.equal(empty.sessionButtons.length, 3);
  await empty.nodes.get('#sessions-close').click();
  assert.equal(empty.panel.hidden, true);
  empty.responses.push({ status: 500, body: { error: { message: 'list unavailable' } } });
  await empty.nodes.get('#sessions-open').click();
  assert.ok(empty.panel.innerHTML.includes('list unavailable'));
  assert.equal(empty.nodes.get('#sessions-open').disabled, false);
  empty.responses.push({ status: 401, body: {} });
  await empty.nodes.get('#sessions-open').click();
  assert.equal(empty.window.location.href, '/login');

  const expired = await browser('VIEWER', false);
  assert.equal(expired.calls.length, 1);
  assert.equal(expired.window.location.href, '/login');
  assert.equal(expired.nodes.get('#sessions-open').disabled, true);
  assert.equal(expired.nodes.get('#logout').disabled, true);
}

run().then(() => console.log('console session interactions passed')).catch(error => { console.error(error); process.exitCode = 1; });
