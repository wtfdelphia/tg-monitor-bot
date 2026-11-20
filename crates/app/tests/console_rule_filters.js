const assert = require('node:assert/strict');
const { listPage } = require('./console_rule_bulk');

function deferred() {
  let resolve;
  const promise = new Promise(callback => { resolve = callback; });
  return { promise, resolve };
}

function filters(state) { return state.nodes.get('#rules-filter-form'); }
function choices(state) { return state.groups.get('[data-rule-select]'); }
function values(state) { return Object.fromEntries(filters(state).elements.filter(input => input.name).map(input => [input.name, input.value])); }
function setFilters(state, fields) {
  for (const input of filters(state).elements) {
    if (Object.hasOwn(fields, input.name)) input.value = fields[input.name];
  }
}
function page(ids, pageNumber = 1, hasMore = false) {
  return { status: 200, body: {
    items: ids.map(id => ({ id, keyword: `filtered-${id}`, target_tg_chat_id: -10031, action: 'COPY', priority: 1, enabled: false })),
    page: pageNumber, has_more: hasMore,
  } };
}
function assertRead(request, expected) {
  const params = new URLSearchParams(request.url.split('?')[1]);
  assert.deepEqual(Object.fromEntries(params), expected);
  assert.equal(request.url.split('?')[0], '/api/v1/rules');
  assert.equal(request.options.credentials, 'same-origin');
  assert.equal(request.options.method, undefined);
  assert.equal(request.options.body, undefined);
  assert.equal(params.has('tenant_id'), false);
}
function assertUnlocked(state) {
  assert.ok(filters(state).elements.every(input => !input.disabled));
  assert.ok(choices(state).every(input => !input.disabled));
  const bulk = state.nodes.get('#rules-bulk-form');
  if (bulk) assert.ok(bulk.elements.every(input => !input.disabled));
}

async function run() {
  const state = await listPage();
  assertRead(state.readCalls.at(-1), { page: '1' });
  choices(state)[0].checked = true;
  await choices(state)[0].fire('change');
  setFilters(state, { enabled: 'false', target_ref: ' 31 ', source_ref: ' 9007199254740993 ' });
  const pending = deferred();
  const oldForm = filters(state);
  const oldMore = state.nodes.get('#rules-load-more');
  const oldChoices = [...choices(state)];
  const oldBulk = state.nodes.get('#rules-bulk-form');
  state.readResponses.push(pending.promise);
  const loading = oldForm.fire('submit');
  const readsDuring = state.reads.length;
  assertRead(state.readCalls.at(-1), { page: '1', enabled: 'false', target_ref: '31', source_ref: '9007199254740993' });
  assert.ok(oldForm.elements.every(input => input.disabled));
  assert.ok(oldChoices.every(input => input.disabled));
  assert.ok(oldBulk.elements.every(input => input.disabled));
  assert.equal(oldMore.disabled, true);
  await oldForm.fire('submit');
  await state.nodes.get('#rules-filter-clear').fire('click');
  await oldMore.fire('click');
  await oldBulk.fire('submit', { submitter: oldBulk.elements[0] });
  assert.equal(state.reads.length, readsDuring);
  assert.equal(state.writes.length, 0);
  pending.resolve(page([71, 72], 1, true));
  await loading;
  assert.deepEqual(choices(state).map(input => input.dataset.ruleSelect), ['71', '72']);
  assert.ok(choices(state).every(input => !input.checked));
  assert.deepEqual(values(state), { enabled: 'false', target_ref: '31', source_ref: '9007199254740993' });
  assert.match(state.nodes.get('#rules-selection-count').textContent, /已选 0 \/ 2/);
  assertUnlocked(state);

  choices(state)[0].checked = true;
  await choices(state)[0].fire('change');
  const morePending = deferred();
  state.readResponses.push(morePending.promise);
  const more = state.nodes.get('#rules-load-more').fire('click');
  const moreReads = state.reads.length;
  assertRead(state.readCalls.at(-1), { page: '2', enabled: 'false', target_ref: '31', source_ref: '9007199254740993' });
  await filters(state).fire('submit');
  await state.nodes.get('#rules-filter-clear').fire('click');
  await state.nodes.get('#rules-load-more').fire('click');
  assert.equal(state.reads.length, moreReads);
  morePending.resolve(page([73], 2));
  await more;
  assert.deepEqual(choices(state).map(input => input.dataset.ruleSelect), ['71', '72', '73']);
  assert.equal(choices(state)[0].checked, true);
  assert.match(state.nodes.get('#rules-selection-count').textContent, /已选 1 \/ 3/);
  assertUnlocked(state);

  const writePending = deferred();
  state.responses.push(writePending.promise);
  const bulk = state.nodes.get('#rules-bulk-form');
  const writing = bulk.fire('submit', { submitter: bulk.elements[0] });
  const writeReads = state.reads.length;
  assert.ok(filters(state).elements.every(input => input.disabled));
  await filters(state).fire('submit');
  await state.nodes.get('#rules-filter-clear').fire('click');
  assert.equal(state.reads.length, writeReads);
  assert.equal(state.writes.length, 1);
  assert.equal(state.writes[0].url, '/api/v1/rules/71');
  assert.deepEqual(JSON.parse(state.writes[0].options.body), { enabled: true });
  writePending.resolve({ status: 200, body: {} });
  await writing;
  assertUnlocked(state);

  state.readResponses.push(page([41, 42], 1, true));
  await state.nodes.get('#rules-filter-clear').fire('click');
  assertRead(state.readCalls.at(-1), { page: '1' });
  assert.deepEqual(values(state), { enabled: '', target_ref: '', source_ref: '' });
  assert.ok(choices(state).every(input => !input.checked));
  await state.nodes.get('#rules-load-more').fire('click');
  assertRead(state.readCalls.at(-1), { page: '2' });
  assert.equal(choices(state).length, 3);

  const failed = await listPage();
  setFilters(failed, { enabled: 'true', target_ref: '31' });
  failed.readResponses.push(page([81, 82], 1, true));
  await filters(failed).fire('submit');
  choices(failed)[0].checked = true;
  await choices(failed)[0].fire('change');
  setFilters(failed, { enabled: 'false', target_ref: 'bad&tenant_id=999', source_ref: 'x' });
  const failureForm = filters(failed);
  failureForm.elements[1].disabled = true;
  const previousDisabled = failureForm.elements.map(input => input.disabled);
  const previousValues = values(failed);
  const oldIds = choices(failed).map(input => input.dataset.ruleSelect);
  for (const response of [
    { status: 400, body: { error: { message: '<invalid>' } } },
    { error: new Error('<network>') },
    { status: 200, jsonError: new Error('<parse>') },
  ]) {
    failed.readResponses.push(response);
    await failureForm.fire('submit');
    assert.deepEqual(choices(failed).map(input => input.dataset.ruleSelect), oldIds);
    assert.equal(choices(failed)[0].checked, true);
    assert.deepEqual(values(failed), previousValues);
    assert.deepEqual(failureForm.elements.map(input => input.disabled), previousDisabled);
    assert.equal(failed.nodes.get('#rules-load-more').disabled, false);
    assert.ok(choices(failed).every(input => !input.disabled));
    assert.equal(failed.nodes.get('#content').innerHTML.includes('<invalid>'), false);
    assert.equal(failed.nodes.get('#content').innerHTML.includes('<network>'), false);
    assert.equal(failed.nodes.get('#content').innerHTML.includes('<parse>'), false);
  }
  failed.readResponses.push({ status: 500, body: { error: { message: 'clear failed' } } });
  await failed.nodes.get('#rules-filter-clear').fire('click');
  assertRead(failed.readCalls.at(-1), { page: '1' });
  assert.deepEqual(choices(failed).map(input => input.dataset.ruleSelect), oldIds);
  assert.equal(choices(failed)[0].checked, true);
  assert.deepEqual(values(failed), previousValues);
  assert.deepEqual(failureForm.elements.map(input => input.disabled), previousDisabled);
  await failed.nodes.get('#rules-load-more').fire('click');
  assertRead(failed.readCalls.at(-1), { page: '2', enabled: 'true', target_ref: '31' });
  assert.equal(choices(failed)[0].checked, true);
  failureForm.elements[1].disabled = false;
  failed.readResponses.push(page([83]));
  await failureForm.fire('submit');
  assertRead(failed.readCalls.at(-1), { page: '1', enabled: 'false', target_ref: 'bad&tenant_id=999', source_ref: 'x' });
  assert.equal(choices(failed)[0].checked, false);

  const escaped = await listPage();
  setFilters(escaped, { source_ref: '"><img src=x onerror=alert(1)>' });
  escaped.readResponses.push(page([]));
  await filters(escaped).fire('submit');
  assert.match(escaped.nodes.get('#content').innerHTML, /&quot;&gt;&lt;img/);
  assert.equal(escaped.nodes.get('#content').innerHTML.includes('<img'), false);
  assert.match(escaped.nodes.get('#content').innerHTML, /暂无规则/);
  assert.equal(choices(escaped).length, 0);

  for (const role of ['VIEWER', 'MEMBER']) {
    const readonly = await listPage(role);
    assert.ok(filters(readonly));
    setFilters(readonly, { enabled: 'true' });
    readonly.readResponses.push(page([91]));
    await filters(readonly).fire('submit');
    assertRead(readonly.readCalls.at(-1), { page: '1', enabled: 'true' });
    assert.equal(readonly.nodes.has('#rules-bulk-form'), false);
    assert.equal(choices(readonly).length, 0);
    assert.equal(readonly.writes.length, 0);
    assertUnlocked(readonly);
  }
  const expired = await listPage();
  expired.readResponses.push({ status: 401, body: {} });
  await filters(expired).fire('submit');
  assert.equal(expired.window.location.href, '/login');
  assert.equal(expired.writes.length, 0);
  assertUnlocked(expired);
}

run().catch(error => { console.error(error); process.exitCode = 1; });
