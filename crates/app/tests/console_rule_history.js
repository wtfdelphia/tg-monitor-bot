const assert = require('node:assert/strict');
const { browser, element } = require('./console_writes');

function deferred() {
  let resolve;
  const promise = new Promise(callback => { resolve = callback; });
  return { promise, resolve };
}

async function editor(role = 'TENANT_OWNER') {
  const state = await browser('/console/rules/41', role);
  const buttons = [];
  const list = state.nodes.get('#rule-history-list');
  let listHtml = '';
  Object.defineProperty(list, 'innerHTML', {
    get: () => listHtml,
    set: html => {
      listHtml = html;
      buttons.length = 0;
      for (const match of html.matchAll(/data-rule-version="([^"]+)"/g)) {
        buttons.push(element('button', { ruleVersion: match[1] }));
      }
    },
  });
  state.groups.set('[data-rule-version]', buttons);
  return { ...state, buttons };
}

function versions(items = [
  { version: 2, created_at: '2026-10-12T09:00:00Z' },
  { version: 1, created_at: '<img src=x onerror=alert(1)>' },
]) {
  return { status: 200, body: { items } };
}

function detail(version = 1) {
  return {
    status: 200,
    body: {
      rule_id: 41, version, created_at: '2026-10-12T09:00:00Z',
      snapshot: {
        rule: { id: 41, tenant_id: 987654, keyword_id: 21, target_ref: 31, action: 'COPY', priority: 3, template: '</pre><img src=x onerror=alert(1)>', version, enabled: false, secret: 'internal-only' },
        conditions: [{ kind: 'exclude_word', value: '<script>alert(1)</script>', tenant_id: 987654 }],
        replacements: [{ seq: 0, pattern: 'foo', replacement: 'bar', enabled: true, tenant_id: 987654 }],
        internal: 'internal-only',
      },
    },
  };
}

async function loadVersions(state, response = versions()) {
  state.readResponses.push(response);
  await state.nodes.get('#rule-history-refresh').fire('click');
}

function assertFormUnchanged(state, values, disabled) {
  const inputs = state.nodes.get('#rule-form').elements;
  assert.deepEqual(inputs.map(input => input.value), values);
  assert.deepEqual(inputs.map(input => input.disabled), disabled);
  assert.equal(state.writes.length, 0);
  assert.equal(state.window.location.href, '');
}

async function run() {
  const state = await editor();
  const refresh = state.nodes.get('#rule-history-refresh');
  const list = state.nodes.get('#rule-history-list');
  const message = state.nodes.get('#rule-history-message');
  const snapshot = state.nodes.get('#rule-history-snapshot');
  const form = state.nodes.get('#rule-form');
  form.elements.find(input => input.name === 'template').value = 'unsaved template';
  const values = form.elements.map(input => input.value);
  const disabled = form.elements.map(input => input.disabled);
  assert.ok(state.nodes.get('#content').innerHTML.includes('版本历史'));
  assert.equal(state.reads.some(url => url.includes('/versions')), false);
  assert.equal(snapshot.hidden, true);

  const loading = deferred();
  const countBefore = state.reads.length;
  state.readResponses.push(loading.promise);
  const initial = refresh.fire('click');
  assert.equal(refresh.disabled, true);
  assert.match(message.textContent, /正在加载/);
  await refresh.fire('click');
  assert.equal(state.reads.length, countBefore + 1);
  loading.resolve(versions());
  await initial;
  assert.equal(refresh.disabled, false);
  assert.equal(state.buttons.length, 2);
  assert.match(list.innerHTML, /2（当前编辑版本）/);
  assert.match(list.innerHTML, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.equal(list.innerHTML.includes('<img'), false);
  assert.equal(state.reads.at(-1), '/api/v1/rules/41/versions');

  const selected = state.buttons.find(button => button.dataset.ruleVersion === '1');
  const other = state.buttons.find(button => button.dataset.ruleVersion === '2');
  const selecting = deferred();
  state.readResponses.push(selecting.promise);
  const opening = selected.fire('click');
  assert.equal(selected.disabled, true);
  assert.equal(refresh.disabled, true);
  const countDuring = state.reads.length;
  await selected.fire('click');
  await other.fire('click');
  await refresh.fire('click');
  assert.equal(state.reads.length, countDuring);
  selecting.resolve(detail());
  await opening;
  assert.equal(refresh.disabled, false);
  assert.equal(selected.disabled, false);
  assert.equal(snapshot.hidden, false);
  assert.equal(snapshot.innerHTML, '');
  assert.equal(state.reads.at(-1), '/api/v1/rules/41/versions/1');
  for (const request of state.readCalls.filter(item => item.url.includes('/versions'))) {
    assert.equal(request.options.credentials, 'same-origin');
    assert.equal(request.options.method, undefined);
    assert.equal(request.options.body, undefined);
    assert.equal(request.url.includes('tenant'), false);
  }
  const displayed = JSON.parse(snapshot.textContent);
  assert.equal(displayed.rule.template, detail().body.snapshot.rule.template);
  assert.equal(displayed.conditions[0].value, '<script>alert(1)</script>');
  assert.equal(displayed.replacements[0].replacement, 'bar');
  assert.equal(snapshot.textContent.includes('tenant_id'), false);
  assert.equal(snapshot.textContent.includes('987654'), false);
  assert.equal(snapshot.textContent.includes('internal-only'), false);
  assertFormUnchanged(state, values, disabled);

  const oldList = list.innerHTML;
  const oldSnapshot = snapshot.textContent;
  await loadVersions(state, { status: 500, body: { error: { message: '<list error>' } } });
  assert.equal(refresh.disabled, false);
  assert.equal(message.textContent, '<list error>');
  assert.equal(message.innerHTML, '');
  assert.equal(list.innerHTML, oldList);
  assert.equal(snapshot.textContent, oldSnapshot);
  assertFormUnchanged(state, values, disabled);

  for (const failure of [
    { status: 404, body: { error: { message: 'not found' } } },
    { status: 500, body: { error: { message: 'unavailable' } } },
    { error: new Error('network') },
    { status: 200, jsonError: new Error('invalid JSON') },
    { status: 200, body: { ...detail().body, rule_id: 42, snapshot: { ...detail().body.snapshot, rule: { ...detail().body.snapshot.rule, template: 'wrong rule snapshot' } } } },
    detail(3),
    { status: 200, body: { ...detail().body, snapshot: null } },
    { status: 200, body: { ...detail().body, snapshot: { rule: {}, conditions: null, replacements: [] } } },
    { status: 200, body: { ...detail().body, snapshot: { rule: {}, conditions: [null], replacements: [] } } },
  ]) {
    state.readResponses.push(failure);
    await selected.fire('click');
    assert.equal(selected.disabled, false);
    assert.equal(refresh.disabled, false);
    assert.ok(message.textContent);
    assert.equal(snapshot.textContent, oldSnapshot);
    assertFormUnchanged(state, values, disabled);
  }
  state.readResponses.push(detail(2));
  await other.fire('click');
  assert.equal(JSON.parse(snapshot.textContent).rule.version, 2);
  assert.match(message.textContent, /版本 2/);
  assertFormUnchanged(state, values, disabled);

  for (const failure of [
    { error: new Error('network') },
    { status: 200, jsonError: new Error('invalid JSON') },
    { status: 200, body: {} },
    versions([{ version: '1', created_at: '' }]),
    versions([{ version: 0, created_at: '' }]),
  ]) {
    await loadVersions(state, failure);
    assert.equal(refresh.disabled, false);
    assert.ok(message.textContent);
    assert.equal(state.buttons.length, 2);
  }
  await loadVersions(state, versions([]));
  assert.match(list.innerHTML, /暂无版本历史/);
  assert.equal(snapshot.textContent, '');
  assert.equal(snapshot.hidden, true);
  assert.equal(state.buttons.length, 0);
  await loadVersions(state);
  assert.equal(state.buttons.length, 2);
  assertFormUnchanged(state, values, disabled);

  const created = await browser('/console/rules/new');
  assert.match(created.nodes.get('#content').innerHTML, /保存规则后可查看版本历史/);
  assert.equal(created.reads.some(url => url.includes('/versions')), false);
  assert.equal(created.nodes.get('#rule-history-refresh').events.has('click'), false);
  for (const role of ['VIEWER', 'MEMBER']) {
    const readonly = await editor(role);
    assert.match(readonly.nodes.get('#content').innerHTML, /当前角色没有规则编辑权限/);
    assert.equal(readonly.reads.length, 1);
    assert.equal(readonly.nodes.get('#rule-history-refresh').events.has('click'), false);
  }
  const administrator = await editor('TENANT_ADMIN');
  await loadVersions(administrator);
  assert.equal(administrator.buttons.length, 2);
  const expired = await editor();
  await loadVersions(expired, { status: 401, body: {} });
  assert.equal(expired.window.location.href, '/login');
  assert.equal(expired.nodes.get('#rule-history-refresh').disabled, false);
}

run().catch(error => { console.error(error); process.exitCode = 1; });
