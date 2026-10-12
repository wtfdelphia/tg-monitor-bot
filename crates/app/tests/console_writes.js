const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const script = fs.readFileSync(0, 'utf8');

function element(tag = 'button', dataset = {}) {
  return {
    tag, dataset, disabled: false, style: {}, value: '', textContent: '', innerHTML: '', children: [],
    events: new Map(),
    matches(selector) { return selector === this.tag; },
    addEventListener(name, handler) { this.events.set(name, handler); },
    insertAdjacentHTML(position, html) { this.innerHTML = html + this.innerHTML; },
    closest() { return this; },
    async fire(name, extra = {}) {
      assert.ok(this.events.has(name), `Missing handler: ${name}`);
      await this.events.get(name)({ currentTarget: this, target: this, preventDefault() {}, ...extra });
    },
  };
}

function form(values, dataset = {}) {
  const node = element('form', dataset);
  node.elements = Object.entries(values).map(([name, value]) => ({ name, value, disabled: false }));
  node.elements.push({ disabled: false }, { disabled: true });
  return node;
}

function deferred() {
  let resolve;
  const promise = new Promise(callback => { resolve = callback; });
  return { promise, resolve };
}

async function browser(path, role = 'TENANT_OWNER', configure = () => {}) {
  const nodes = new Map(['#content', '#title', '#me', '#sessions-open', '#logout', '#sessions-panel'].map(selector => [selector, element()]));
  const groups = new Map();
  const reads = [];
  const readCalls = [];
  const writes = [];
  const responses = [];
  const readResponses = [];
  const alerts = [];
  let fileReads = 0;
  const window = {
    location: { pathname: path, href: '' },
    confirm: () => true,
    prompt: () => 'test text',
    alert: message => alerts.push(message),
  };
  function group(selector, node) { groups.set(selector, [node]); return node; }
  function named(selector, node = element()) { nodes.set(selector, node); return node; }
  if (path === '/console/identities') {
    group('[data-identity-reset]', element('button', { identityReset: '11' }));
    group('[data-identity-delete]', element('button', { identityDelete: '11' }));
    group('[data-identity-edit]', form({ display_name: ' Updated ', proxy_url: '', clear_proxy: 'on' }, { identityEdit: '11' }));
  } else if (path === '/console/keywords') {
    named('#keyword-filter-form', form({ q: '', match_type: '', category: '' }));
    named('#keyword-filter-clear');
    named('#keyword-form', form({ word: 'new word', match_type: 'exact', category: 'category', priority: '7' }));
    named('#keyword-test');
    named('#keyword-export');
    named('#keyword-import-label');
    const input = named('#keyword-import', element('input'));
    input.files = [{ name: 'keywords.csv', text: async () => { fileReads += 1; return 'word,match_type\nhello,exact'; } }];
    input.value = 'keywords.csv';
    group('[data-keyword-toggle]', element('button', { keywordToggle: '21', enabled: 'true' }));
    group('[data-keyword-delete]', element('button', { keywordDelete: '21' }));
    group('[data-keyword-edit]', form({ word: 'changed word', match_type: 'fuzzy', category: '', priority: '3' }, { keywordEdit: '21' }));
  } else if (path === '/console/targets') {
    named('#target-form', form({ chat: '-1001234567890', title: 'Target', sender_identity_id: '11' }));
    group('[data-target-toggle]', element('button', { targetToggle: '31', enabled: 'true' }));
    group('[data-target-delete]', element('button', { targetDelete: '31' }));
    group('[data-target-edit]', form({ chat: '-1001234567890', title: 'Changed', sender_identity_id: '11' }, { targetEdit: '31' }));
  } else {
    if (path === '/console/rules') {
      const filters = named('#rules-filter-form', form({ enabled: '', target_ref: '', source_ref: '' }));
      filters.elements.push(named('#rules-filter-clear'));
    }
    named('#rule-form', form({ keyword_id: '21', target_ref: '31', action: 'COPY', priority: '9', source_ref: '', media_type: 'TEXT', template: '{text}', stop_processing: 'on', enabled: 'on' }));
    named('[name=action]');
    named('[name=media_type]');
    named('#condition-list', element('div'));
    named('#replacement-list', element('div'));
    named('#add-condition');
    named('#add-replacement');
    named('#rule-history-refresh');
    named('#rule-history-list', element('div'));
    named('#rule-history-message', element('p'));
    named('#rule-history-snapshot', element('pre'));
    nodes.get('#rule-history-snapshot').hidden = true;
  }
  const context = {
    window, URLSearchParams,
    document: {
      cookie: 'tgm_csrf=csrf-token',
      querySelector: selector => nodes.get(selector) || null,
      querySelectorAll: selector => groups.get(selector) || (nodes.has(selector) ? [nodes.get(selector)] : []),
    },
    FormData: class {
      constructor(node) { this.values = new Map(node.elements.filter(input => !input.disabled && input.name).map(input => [input.name, input.value])); }
      get(name) { return this.values.get(name) ?? null; }
    },
    fetch: async (url, options = {}) => {
      let response;
      if (options.method) {
        writes.push({ url, options });
        assert.ok(responses.length, `Unexpected write: ${url}`);
        response = await responses.shift();
      } else {
        reads.push(url);
        readCalls.push({ url, options });
        if (readResponses.length) response = await readResponses.shift();
        else if (url === '/auth/me') response = { status: 200, body: { user_id: 7100, role } };
        else if (url.includes('/identities?')) response = { status: 200, body: { items: [{ id: 11, display_name: 'Identity', kind: 'USER', status: 'online' }], has_more: false } };
        else if (url.includes('/keywords?')) response = { status: 200, body: { items: [{ id: 21, word: 'word', match_type: 'exact', priority: 1, enabled: true }], has_more: false } };
        else if (url.includes('/targets?')) response = { status: 200, body: { items: [{ id: 31, title: 'Target', tg_chat_id: -1001234567890, sender_identity_id: 11, enabled: true }], has_more: false } };
        else if (url.startsWith('/api/v1/rules?page=')) {
          const page = Number(new URLSearchParams(url.split('?')[1]).get('page'));
          response = { status: 200, body: { items: (page === 1 ? [41, 42] : [43]).map(id => ({ id, keyword: `word-${id}`, target_tg_chat_id: -1001234567890, action: 'COPY', priority: 1, enabled: id === 41 })), page, has_more: page === 1 } };
        }
        else if (url === '/api/v1/rules/41') response = { status: 200, body: { rule: { id: 41, keyword_id: 21, target_ref: 31, action: 'COPY', version: 2 }, conditions: [], replacements: [] } };
        else throw new Error(`Unexpected read: ${url}`);
      }
      if (response.error) throw response.error;
      return {
        status: response.status,
        ok: response.status >= 200 && response.status < 300,
        headers: { get: () => response.contentType ?? 'application/json' },
        text: async () => response.text ?? '',
        json: async () => {
          if (response.jsonError) throw response.jsonError;
          return response.body;
        },
      };
    },
  };
  const state = { nodes, groups, reads, readCalls, writes, responses, readResponses, window, context, alerts, fileReads: () => fileReads };
  configure(state);
  vm.runInNewContext(script, context, { timeout: 1000 });
  await new Promise(resolve => setImmediate(resolve));
  return state;
}

const scenarios = [
  { path: '/console/identities', selector: '[data-identity-reset]', event: 'click', method: 'POST', url: '/api/v1/identities/11/reset-status' },
  { path: '/console/identities', selector: '[data-identity-delete]', event: 'click', method: 'DELETE', url: '/api/v1/identities/11', confirm: true },
  { path: '/console/identities', selector: '[data-identity-edit]', event: 'submit', method: 'PATCH', url: '/api/v1/identities/11', body: { display_name: 'Updated', proxy_url: null } },
  { path: '/console/keywords', selector: '#keyword-form', event: 'submit', method: 'POST', url: '/api/v1/keywords', body: { word: 'new word', match_type: 'exact', category: 'category', priority: 7, enabled: true } },
  { path: '/console/keywords', selector: '#keyword-test', event: 'click', method: 'POST', url: '/api/v1/keywords/test', body: { text: 'test text' }, prompt: true },
  { path: '/console/keywords', selector: '#keyword-import', event: 'change', method: 'POST', url: '/api/v1/keywords/import', raw: 'word,match_type\nhello,exact' },
  { path: '/console/keywords', selector: '[data-keyword-toggle]', event: 'click', method: 'PATCH', url: '/api/v1/keywords/21', body: { enabled: false } },
  { path: '/console/keywords', selector: '[data-keyword-delete]', event: 'click', method: 'DELETE', url: '/api/v1/keywords/21', confirm: true },
  { path: '/console/keywords', selector: '[data-keyword-edit]', event: 'submit', method: 'PATCH', url: '/api/v1/keywords/21', body: { word: 'changed word', match_type: 'fuzzy', category: null, priority: 3 } },
  { path: '/console/targets', selector: '#target-form', event: 'submit', method: 'POST', url: '/api/v1/targets', body: { chat: '-1001234567890', title: 'Target', sender_identity_id: 11 } },
  { path: '/console/targets', selector: '[data-target-toggle]', event: 'click', method: 'PATCH', url: '/api/v1/targets/31', body: { enabled: false } },
  { path: '/console/targets', selector: '[data-target-delete]', event: 'click', method: 'DELETE', url: '/api/v1/targets/31', confirm: true },
  { path: '/console/targets', selector: '[data-target-edit]', event: 'submit', method: 'PATCH', url: '/api/v1/targets/31', body: { chat: '-1001234567890', title: 'Changed', sender_identity_id: 11 } },
  { path: '/console/rules/new', selector: '#rule-form', event: 'submit', method: 'POST', url: '/api/v1/rules', redirect: true },
  { path: '/console/rules/41', selector: '#rule-form', event: 'submit', method: 'PATCH', url: '/api/v1/rules/41', redirect: true },
];

function target(state, scenario) { return state.nodes.get(scenario.selector) || state.groups.get(scenario.selector)[0]; }
function controls(node) { return node.elements || [node]; }
function assertRestored(node, previous) {
  assert.deepEqual(controls(node).map(control => control.disabled), previous);
  assert.equal(node.dataset.pending, undefined);
}

async function run() {
  for (const [path, selector, field, url] of [
    ['/console/keywords', '[data-keyword-edit]', 'category', '/api/v1/keywords/21'],
    ['/console/targets', '[data-target-edit]', 'title', '/api/v1/targets/31'],
  ]) {
    const state = await browser(path);
    const node = state.groups.get(selector)[0];
    node.elements.find(input => input.name === field).value = '';
    state.responses.push({ status: 200, body: {} });
    await node.fire('submit');
    assert.equal(state.writes.length, 1);
    assert.equal(state.writes[0].url, url);
    assert.equal(state.writes[0].options.method, 'PATCH');
    const payload = JSON.parse(state.writes[0].options.body);
    assert.ok(Object.hasOwn(payload, field), `${field} must not be omitted`);
    assert.equal(payload[field], null);
  }

  const clearing = await browser('/console/rules/41');
  const clearingForm = clearing.nodes.get('#rule-form');
  for (const input of clearingForm.elements) {
    if (['source_ref', 'media_type', 'template'].includes(input.name)) input.value = '';
  }
  clearing.responses.push({ status: 200, body: {} });
  await clearingForm.fire('submit');
  assert.equal(clearing.writes.length, 1);
  assert.equal(clearing.writes[0].url, '/api/v1/rules/41');
  assert.equal(clearing.writes[0].options.method, 'PATCH');
  const clearedPayload = JSON.parse(clearing.writes[0].options.body);
  for (const field of ['source_ref', 'media_type', 'template']) {
    assert.ok(Object.hasOwn(clearedPayload, field), `${field} must not be omitted`);
    assert.equal(clearedPayload[field], null);
  }

  for (const scenario of scenarios) {
    const state = await browser(scenario.path);
    const node = target(state, scenario);
    const previous = controls(node).map(control => control.disabled);
    const inputValues = node.elements?.map(input => input.value);
    const pending = deferred();
    state.responses.push(pending.promise);
    const initialReads = state.reads.length;
    const writing = node.fire(scenario.event);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(state.writes.length, 1, scenario.selector);
    assert.ok(controls(node).every(control => control.disabled), scenario.selector);
    assert.equal(node.dataset.pending, 'true');
    await node.fire(scenario.event);
    assert.equal(state.writes.length, 1, `duplicate: ${scenario.selector}`);
    if (scenario.raw) assert.equal(state.fileReads(), 1);
    const request = state.writes[0];
    assert.equal(request.url, scenario.url);
    assert.equal(request.options.method, scenario.method);
    assert.equal(request.options.credentials, 'same-origin');
    assert.equal(request.options.headers['x-csrf-token'], 'csrf-token');
    if (scenario.body) assert.deepEqual(JSON.parse(request.options.body), scenario.body);
    if (scenario.raw) {
      assert.equal(request.options.body, scenario.raw);
      assert.equal(request.options.headers['content-type'], 'text/csv');
    }
    if (scenario.redirect) assert.equal(JSON.parse(request.options.body).keyword_id, 21);
    pending.resolve({ status: 409, body: { error: { message: '<conflict>' } } });
    await writing;
    assertRestored(node, previous);
    assert.equal(state.reads.length, initialReads);
    assert.equal(state.window.location.href, '');
    assert.match(state.nodes.get('#content').innerHTML, /&lt;conflict&gt;/);
    if (inputValues) assert.deepEqual(node.elements.map(input => input.value), inputValues);
    const retry = deferred();
    state.responses.push(retry.promise);
    const retrying = node.fire(scenario.event);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(state.writes.length, 2);
    retry.resolve({ status: 200, body: { matches: [{ word: 'word' }] } });
    await retrying;
    assertRestored(node, previous);
    if (scenario.redirect) assert.equal(state.window.location.href, '/console/rules');
    else if (scenario.prompt) assert.deepEqual(state.alerts, ['word']);
    else assert.ok(state.reads.length > initialReads, scenario.selector);
    if (scenario.raw) assert.equal(node.value, '');
  }

  for (const scenario of scenarios.filter(item => item.confirm || item.prompt)) {
    const state = await browser(scenario.path);
    const node = target(state, scenario);
    const previous = controls(node).map(control => control.disabled);
    state.window.confirm = () => false;
    state.window.prompt = () => null;
    await node.fire(scenario.event);
    assert.equal(state.writes.length, 0);
    assertRestored(node, previous);
  }

  for (const role of ['VIEWER', 'MEMBER']) {
    const state = await browser('/console/keywords', role);
    const node = state.nodes.get('#keyword-form');
    const previous = controls(node).map(control => control.disabled);
    await node.fire('submit');
    assert.equal(state.writes.length, 0);
    assertRestored(node, previous);
    assert.match(state.nodes.get('#content').innerHTML, /当前角色没有写入权限/);
  }

  for (const response of [
    { status: 403, body: { error: { message: 'forbidden' } } },
    { status: 500, body: { error: { message: 'unavailable' } } },
    { error: new Error('network') },
    { status: 200, jsonError: new Error('invalid JSON') },
  ]) {
    const state = await browser('/console/keywords');
    const node = state.nodes.get('#keyword-form');
    const previous = controls(node).map(control => control.disabled);
    state.responses.push(response);
    await node.fire('submit');
    assertRestored(node, previous);
    assert.equal(state.window.location.href, '');
  }

  const expired = await browser('/console/keywords');
  const expiredForm = expired.nodes.get('#keyword-form');
  const previousExpired = controls(expiredForm).map(control => control.disabled);
  expired.responses.push({ status: 401, body: {} });
  await expiredForm.fire('submit');
  assertRestored(expiredForm, previousExpired);
  assert.equal(expired.window.location.href, '/login');

  const administrator = await browser('/console/keywords', 'TENANT_ADMIN');
  administrator.responses.push({ status: 201, body: {} });
  await administrator.nodes.get('#keyword-form').fire('submit');
  assert.equal(administrator.writes.length, 1);

  const noFile = await browser('/console/keywords');
  noFile.nodes.get('#keyword-import').files = [];
  await noFile.nodes.get('#keyword-import').fire('change');
  assert.equal(noFile.writes.length, 0);

  const importing = await browser('/console/keywords');
  const fileInput = importing.nodes.get('#keyword-import');
  const fileContents = deferred();
  fileInput.files[0].text = () => fileContents.promise;
  const fileRequest = deferred();
  importing.responses.push(fileRequest.promise);
  const readingFile = fileInput.fire('change');
  assert.equal(fileInput.disabled, true);
  assert.equal(importing.writes.length, 0);
  await fileInput.fire('change');
  fileContents.resolve('word,match_type\nhello,exact');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(importing.writes.length, 1);
  fileRequest.resolve({ error: new Error('network') });
  await readingFile;
  assertRestored(fileInput, [false]);
  assert.equal(fileInput.value, '');
  fileInput.files[0].text = async () => { throw new Error('file unavailable'); };
  await fileInput.fire('change');
  assert.equal(importing.writes.length, 1);
  assertRestored(fileInput, [false]);
  assert.match(importing.nodes.get('#content').innerHTML, /file unavailable/);

  const refreshing = await browser('/console/keywords');
  const refreshingForm = refreshing.nodes.get('#keyword-form');
  const previousRefreshing = controls(refreshingForm).map(control => control.disabled);
  const refresh = deferred();
  refreshing.responses.push({ status: 201, body: {} });
  refreshing.readResponses.push(refresh.promise);
  const saving = refreshingForm.fire('submit');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(refreshingForm.dataset.pending, 'true');
  await refreshingForm.fire('submit');
  assert.equal(refreshing.writes.length, 1);
  refresh.resolve({ status: 500, body: { error: { message: 'refresh failed' } } });
  await saving;
  assertRestored(refreshingForm, previousRefreshing);
  assert.match(refreshing.nodes.get('#content').innerHTML, /refresh failed/);

  const independent = await browser('/console/identities');
  const reset = independent.groups.get('[data-identity-reset]')[0];
  const edit = independent.groups.get('[data-identity-edit]')[0];
  const first = deferred();
  const second = deferred();
  independent.responses.push(first.promise, second.promise);
  const resetting = reset.fire('click');
  const editing = edit.fire('submit');
  assert.equal(independent.writes.length, 2);
  first.resolve({ status: 500, body: {} });
  second.resolve({ status: 500, body: {} });
  await Promise.all([resetting, editing]);

  const disabled = await browser('/console/keywords');
  const disabledForm = disabled.nodes.get('#keyword-form');
  const previous = controls(disabledForm).map(control => control.disabled);
  disabled.responses.push({ error: new Error('network') });
  await disabledForm.fire('submit');
  assertRestored(disabledForm, previous);
}

if (require.main === module) run().catch(error => { console.error(error); process.exitCode = 1; });

module.exports = { browser, element };
