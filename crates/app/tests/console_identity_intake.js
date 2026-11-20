'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync(0, 'utf8');
const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256"><path d="M0 0h256v256H0z"/></svg>';
const loginId = 'A'.repeat(43);

function element(type = '') {
  return {
    type, value: '', checked: false, disabled: false, hidden: false, dataset: {}, events: {}, style: {}, textContent: '',
    addEventListener(name, handler) { this.events[name] = handler; },
    removeAttribute(name) { delete this[name]; },
    matches(selector) { return selector === 'form' && Boolean(this.elements); },
    focus() {},
    async fire(name) { return this.events[name]({ currentTarget: this, preventDefault() {} }); },
  };
}

function form(fields) {
  const result = element();
  result.elements = Object.entries(fields).map(([name, type]) => Object.assign(element(type), { name }));
  result.elements.namedItem = name => result.elements.find(input => input.name === name);
  return result;
}

function harness(source = script, writable = true) {
  const nodes = new Map([
    ['#bot-intake-form', form({ token: 'password' })],
    ['#identity-qr-form', form({ api_id: 'text', api_hash: 'password', consent: 'checkbox' })],
    ['#identity-password-form', form({ password: 'password' })],
    ...['#identity-qr-image', '#identity-intake-status', '#identity-login-stop', '#identity-reauthorize-label'].map(name => [name, element()]),
  ]);
  nodes.get('#identity-password-form').hidden = true;
  nodes.get('#identity-qr-image').hidden = true;
  const reauthorize = Object.assign(element(), { dataset: { identityReauthorize: '42' } });
  const requests = [], responses = [], timers = new Map(), events = new Map();
  let now = 1000, timerId = 0, refreshed = 0;
  const context = {
    document: {
      querySelector: selector => nodes.get(selector) || null,
      querySelectorAll: selector => selector === '[data-identity-reauthorize]' ? [reauthorize] : [],
    },
    window: { addEventListener: (name, handler) => events.set(name, handler), removeEventListener: name => events.delete(name) },
    Date: { now: () => now, parse: Date.parse },
    AbortController, AbortSignal, encodeURIComponent, DOMParser: class {
      parseFromString(value) {
        return { querySelector: () => value.startsWith('<svg ') ? null : {}, documentElement: { localName: 'svg', namespaceURI: 'http://www.w3.org/2000/svg' } };
      }
    },
    setTimeout: (callback, delay) => { timerId += 1; timers.set(timerId, { callback, delay }); return timerId; },
    clearTimeout: id => timers.delete(id),
    fetch: async (url, options) => {
      assert.equal(options.credentials, 'same-origin');
      assert.equal(options.cache, 'no-store');
      if (options.method === 'POST') assert.equal(options.headers['x-csrf-token'], 'csrf-test');
      assert.equal(nodes.get('#bot-intake-form').elements.namedItem('token').value, '');
      assert.equal(nodes.get('#identity-qr-form').elements.namedItem('api_hash').value, '');
      assert.equal(nodes.get('#identity-password-form').elements.namedItem('password').value, '');
      requests.push({ url, ...options, body: options.body ? JSON.parse(options.body) : undefined });
      const response = await responses.shift();
      assert.ok(response, '缺少测试响应');
      if (response.error) throw new Error(response.error);
      return { ok: !response.status || response.status < 400, status: response.status || 200, json: async () => response.body };
    },
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('const escapeHtml='), source.indexOf('const getCsrf=')), context);
  const definitions = source.slice(source.indexOf('function identityIntakeMarkup('), source.indexOf('async function keywords('));
  const runWrite = source.slice(source.indexOf('async function runWrite('), source.indexOf('async function loadMe('));
  vm.runInContext(`let canWrite=${writable};let identityIntakeCleanup=()=>{};const getCsrf=()=> 'csrf-test';const showError=()=>{throw new Error('不得渲染原始错误')};${runWrite}\n${definitions}`, context);
  context.identities = async () => { refreshed += 1; vm.runInContext('identityIntakeCleanup()', context); };
  context.bindIdentityIntake();
  const state = {
    nodes, requests, responses, timers, events, reauthorize, context,
    refreshed: () => refreshed,
    advance: milliseconds => { now += milliseconds; },
    async tick(delay) {
      const entry = [...timers].find(([, timer]) => timer.delay === delay);
      assert.ok(entry, `缺少 ${delay}ms 定时器`);
      timers.delete(entry[0]);
      now += delay;
      await entry[1].callback();
    },
    start() {
      const node = nodes.get('#identity-qr-form');
      node.elements.namedItem('api_id').value = '12345';
      node.elements.namedItem('api_hash').value = 'b'.repeat(32);
      node.elements.namedItem('consent').checked = true;
      return node.fire('submit');
    },
  };
  return state;
}

const started = () => ({ body: { login_id: loginId, qr_svg: svg, expires_at: new Date(61000).toISOString() } });

async function checks(source = script) {
  for (const writable of [true, false]) {
    const state = harness(source, writable);
    const markup = state.context.identityIntakeMarkup([{ kind: 'bot', id: 1 }, { kind: 'mtproto', id: 42 }]);
    assert.equal(markup.includes('data-identity-reauthorize="1"'), false);
    assert.equal(markup.includes('data-identity-reauthorize="42"'), true);
    assert.equal(markup.includes('checked'), false);
    const form = state.nodes.get('#bot-intake-form');
    form.elements.namedItem('token').value = 'fictional-token';
    state.responses.push({ status: 400, body: { error: { message: 'fictional-token' } } });
    await form.fire('submit');
    assert.equal(state.requests.length, writable ? 1 : 0);
    assert.equal(state.nodes.get('#identity-intake-status').textContent.includes('fictional-token'), false);
    assert.equal(form.elements.namedItem('token').value, '');
  }
  {
    const state = harness(source);
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const form = state.nodes.get('#bot-intake-form');
    form.elements.namedItem('token').value = 'fictional-token';
    const first = form.fire('submit');
    await form.fire('submit');
    assert.equal(state.requests.length, 1);
    assert.equal(form.elements.every(input => input.disabled), true);
    resolve({ body: { id: 9 } });
    await first;
    assert.equal(state.refreshed(), 1);
    assert.equal(form.elements.every(input => !input.disabled), true);
  }
  {
    const state = harness(source);
    const form = state.nodes.get('#identity-qr-form');
    form.elements.namedItem('api_id').value = '12345';
    form.elements.namedItem('api_hash').value = 'b'.repeat(32);
    state.responses.push(started());
    await form.fire('submit');
    assert.equal(state.requests.length, 0);
    assert.equal(form.elements.namedItem('api_hash').value, '');
  }
  {
    const state = harness(source);
    state.responses.push(started());
    await state.reauthorize.fire('click');
    assert.equal(state.nodes.get('#identity-qr-form').elements.namedItem('consent').checked, false);
    await state.start();
    assert.deepEqual(Object.keys(state.requests[0].body).sort(), ['api_hash', 'api_id', 'consent_text_version', 'identity_id']);
    assert.equal(state.requests[0].body.identity_id, 42);
    assert.equal(state.requests[0].body.consent_text_version, 'custody-v0');
    assert.equal(state.nodes.get('#identity-qr-image').hidden, false);
    assert.ok(state.nodes.get('#identity-qr-image').src.startsWith('data:image/svg+xml;'));
    state.responses.push({ body: { status: 'pending', qr_svg: svg.replace('256', '255'), expires_at: new Date(91000).toISOString() } });
    await state.tick(25000);
    assert.ok(state.nodes.get('#identity-qr-image').src.includes('255'));
    assert.equal(state.requests[1].url, '/api/v1/identities/login/qr/' + loginId);
    state.responses.push({ body: { status: 'awaiting_password' } });
    await state.tick(25000);
    const password = state.nodes.get('#identity-password-form');
    assert.equal(password.hidden, false);
    assert.equal(state.nodes.get('#identity-qr-image').src, undefined);
    assert.equal([...state.timers.values()].some(timer => timer.delay === 25000), false);
    state.advance(2000);
    password.elements.namedItem('password').value = 'wrong-password';
    state.responses.push({ body: { status: 'awaiting_password', password_rejected: true } });
    await password.fire('submit');
    assert.ok(state.nodes.get('#identity-intake-status').textContent.includes('密码错误'));
    assert.equal(password.hidden, false);
    state.advance(2000);
    password.elements.namedItem('password').value = 'right-password';
    state.responses.push({ body: { status: 'confirmed' } });
    await password.fire('submit');
    assert.equal(state.refreshed(), 1);
    assert.equal(state.timers.size, 0);
  }
  for (const result of [{ status: 'expired' }, { status: 'failed', reason: 'account_mismatch' }, { status: 'unknown' }]) {
    const state = harness(source);
    state.responses.push(started());
    await state.start();
    state.responses.push({ body: result });
    await state.tick(25000);
    assert.equal(state.timers.size, 0);
    assert.equal(state.nodes.get('#identity-qr-image').src, undefined);
  }
  for (const ending of ['pagehide', 'stop', 'timeout']) {
    const state = harness(source);
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const starting = state.start();
    if (ending === 'pagehide') state.events.get('pagehide')();
    else if (ending === 'timeout') await state.tick(600000);
    else await state.nodes.get('#identity-login-stop').fire('click');
    resolve(started());
    await starting;
    assert.equal(state.nodes.get('#identity-qr-image').src, undefined);
    assert.equal(state.timers.size, 0);
    assert.equal(state.requests[0].signal.aborted, true);
  }
  {
    const state = harness(source);
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const old = state.start();
    await state.nodes.get('#identity-login-stop').fire('click');
    resolve(started());
    await old;
    state.responses.push({ body: { ...started().body, login_id: 'B'.repeat(43) } });
    await state.start();
    assert.equal(state.nodes.get('#identity-qr-image').hidden, false);
    await state.nodes.get('#identity-login-stop').fire('click');
    assert.equal(state.timers.size, 0);
  }
  {
    const state = harness(source);
    state.responses.push(started());
    await state.start();
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const polling = state.tick(25000);
    assert.equal([...state.timers.values()].some(timer => timer.delay === 25000), false);
    await state.nodes.get('#identity-login-stop').fire('click');
    state.responses.push({ body: { ...started().body, login_id: 'B'.repeat(43), expires_at: new Date(91000).toISOString() } });
    await state.start();
    const currentImage = state.nodes.get('#identity-qr-image').src;
    resolve({ body: { status: 'awaiting_password' } });
    await polling;
    assert.equal(state.nodes.get('#identity-password-form').hidden, true);
    assert.equal(state.nodes.get('#identity-qr-image').src, currentImage);
    assert.equal([...state.timers.values()].filter(timer => timer.delay === 25000).length, 1);
  }
  for (const response of [
    { status: 429, body: { error: { message: 'sensitive-raw-error' } } },
    { error: 'sensitive-network-error' },
    { body: { ...started().body, qr_svg: '<html>bad</html>' } },
    { body: { ...started().body, expires_at: 'invalid' } },
  ]) {
    const state = harness(source);
    state.responses.push(response);
    await state.start();
    assert.equal(state.timers.size, 0);
    assert.equal(state.nodes.get('#identity-qr-image').src, undefined);
    assert.equal(state.nodes.get('#identity-intake-status').textContent.includes('sensitive-'), false);
    assert.equal(state.nodes.get('#identity-qr-form').elements.namedItem('api_hash').value, '');
  }
  {
    const state = harness(source);
    state.responses.push(started());
    await state.start();
    await state.tick(60000);
    assert.equal(state.nodes.get('#identity-qr-image').src, undefined);
  }
}

async function main() {
  await checks();
  for (const [before, after] of [
    ["const token=input.value;input.value='';", 'const token=input.value;'],
    ["const apiHash=hash.value;hash.value='';", 'const apiHash=hash.value;'],
    ["const password=input.value;input.value='';", 'const password=input.value;'],
    ['if(botForm.dataset.pending)return;', ''],
    ["if(!qrForm.elements.namedItem('consent').checked)", 'if(false)'],
    ['if(current!==generation)return;', ''],
    ['generation+=1;controller.abort();', 'controller.abort();'],
    ['clearTimeout(timer);clearTimeout(expiryTimer);clearImage();', 'clearImage();'],
    ["if(!canWrite)throw new Error('当前角色不能接入身份。');", ''],
  ]) {
    assert.ok(script.includes(before), before);
    let mutant = script.replaceAll(before, after);
    if (before === 'if(botForm.dataset.pending)return;') mutant = mutant.replace('if(control.dataset.pending)return;', '');
    await assert.rejects(() => checks(mutant), `反证未被捕获：${before}`);
  }
  console.log('身份接入交互及反证通过');
}

main().catch(error => { console.error(error); process.exitCode = 1; });
