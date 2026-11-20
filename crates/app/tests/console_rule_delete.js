const assert = require('node:assert/strict');
const { browser, element } = require('./console_writes');

function deferred() {
  let resolve;
  const promise = new Promise(callback => { resolve = callback; });
  return { promise, resolve };
}

async function editor(path = '/console/rules/41', role = 'TENANT_OWNER') {
  return browser(path, role, state => {
    const content = state.nodes.get('#content');
    let html = '';
    Object.defineProperty(content, 'innerHTML', {
      get: () => html,
      set: value => {
        html = value;
        state.nodes.delete('#rule-delete');
        state.nodes.delete('#rule-delete-message');
        if (value.includes('id="rule-delete"')) {
          assert.match(value, /id="rule-delete"[^>]*type="button"/);
          const button = element();
          state.nodes.set('#rule-delete', button);
          state.nodes.set('#rule-delete-message', element('p'));
          state.nodes.get('#rule-form').elements.push(button);
        }
      },
    });
    content.insertAdjacentHTML = (position, value) => { html = position === 'beforeend' ? html + value : value + html; };
  });
}

function form(state) { return state.nodes.get('#rule-form'); }
function button(state) { return state.nodes.get('#rule-delete'); }
function message(state) { return state.nodes.get('#rule-delete-message'); }
function values(state) { return form(state).elements.map(input => input.value); }
function disabled(state) { return form(state).elements.map(input => input.disabled); }
function assertRestored(state, previousValues, previousDisabled) {
  assert.deepEqual(values(state), previousValues);
  assert.deepEqual(disabled(state), previousDisabled);
  assert.equal(form(state).dataset.pending, undefined);
}
function assertDeleteRequest(request) {
  assert.equal(request.url, '/api/v1/rules/41');
  assert.equal(request.options.method, 'DELETE');
  assert.equal(request.options.credentials, 'same-origin');
  assert.equal(request.options.headers['x-csrf-token'], 'csrf-token');
  assert.equal(request.options.body, undefined);
  assert.equal(request.url.includes('tenant'), false);
}

async function run() {
  const canceled = await editor();
  assert.ok(button(canceled));
  const cancelValues = values(canceled);
  const cancelDisabled = disabled(canceled);
  const cancelReads = canceled.reads.length;
  let confirmation = '';
  canceled.window.confirm = text => { confirmation = text; return false; };
  await button(canceled).fire('click');
  assert.match(confirmation, /规则 41/);
  assert.match(confirmation, /条件、替换和版本历史/);
  assert.match(confirmation, /不能撤销/);
  assert.match(confirmation, /已入队任务不会因此取消/);
  assert.equal(canceled.writes.length, 0);
  assert.equal(canceled.reads.length, cancelReads);
  assert.equal(canceled.window.location.href, '');
  assertRestored(canceled, cancelValues, cancelDisabled);

  const deleting = await editor();
  form(deleting).elements.find(input => input.name === 'template').value = 'unsaved template';
  const deleteValues = values(deleting);
  const deleteDisabled = disabled(deleting);
  const deleteReads = deleting.reads.length;
  const pending = deferred();
  deleting.responses.push(pending.promise);
  let confirms = 0;
  deleting.window.confirm = () => { confirms += 1; return true; };
  const deletion = button(deleting).fire('click');
  assert.equal(deleting.writes.length, 1);
  assert.equal(form(deleting).dataset.pending, 'true');
  assert.ok(form(deleting).elements.every(input => input.disabled));
  assert.match(message(deleting).textContent, /正在删除/);
  await button(deleting).fire('click');
  await form(deleting).fire('submit');
  assert.equal(deleting.writes.length, 1);
  assert.equal(confirms, 1);
  assert.equal(deleting.window.location.href, '');
  pending.resolve({ status: 204, contentType: '', text: '' });
  await deletion;
  assertDeleteRequest(deleting.writes[0]);
  assert.equal(deleting.window.location.href, '/console/rules');
  assert.equal(deleting.reads.length, deleteReads);
  assertRestored(deleting, deleteValues, deleteDisabled);

  const saving = await editor();
  const saveValues = values(saving);
  const saveDisabled = disabled(saving);
  const savePending = deferred();
  saving.responses.push(savePending.promise);
  let deleteConfirmation = false;
  saving.window.confirm = () => { deleteConfirmation = true; return true; };
  const save = form(saving).fire('submit');
  assert.equal(saving.writes.length, 1);
  assert.equal(saving.writes[0].options.method, 'PATCH');
  assert.equal(button(saving).disabled, true);
  await button(saving).fire('click');
  assert.equal(saving.writes.length, 1);
  assert.equal(deleteConfirmation, false);
  savePending.resolve({ status: 409, body: { error: { message: '<save conflict>' } } });
  await save;
  assertRestored(saving, saveValues, saveDisabled);
  assert.equal(saving.window.location.href, '');
  saving.responses.push({ status: 204 });
  await button(saving).fire('click');
  assert.equal(saving.writes.length, 2);
  assertDeleteRequest(saving.writes[1]);
  assert.equal(saving.window.location.href, '/console/rules');

  for (const response of [
    { status: 403, body: { error: { message: '<forbidden>' } } },
    { status: 404, body: { error: { message: '<not found>' } } },
    { status: 500, body: { error: { message: '<server failure>' } } },
    { error: new Error('<network>') },
    { status: 200, jsonError: new Error('<parse>') },
  ]) {
    const failed = await editor();
    form(failed).elements.find(input => input.name === 'template').value = '<unsaved>';
    const previousValues = values(failed);
    const previousDisabled = disabled(failed);
    const reads = failed.reads.length;
    failed.responses.push(response);
    await button(failed).fire('click');
    assert.equal(failed.writes.length, 1);
    assertDeleteRequest(failed.writes[0]);
    assert.equal(failed.window.location.href, '');
    assert.equal(failed.reads.length, reads);
    assertRestored(failed, previousValues, previousDisabled);
    assert.match(message(failed).textContent, /删除未确认.*刷新列表核对/);
    assert.equal(message(failed).innerHTML, '');
    failed.responses.push({ status: 204 });
    await button(failed).fire('click');
    assert.equal(failed.writes.length, 2);
    assert.equal(failed.window.location.href, '/console/rules');
  }

  const expired = await editor();
  const expiredValues = values(expired);
  const expiredDisabled = disabled(expired);
  expired.responses.push({ status: 401, body: {} });
  await button(expired).fire('click');
  assert.equal(expired.window.location.href, '/login');
  assert.equal(expired.writes.length, 1);
  assertRestored(expired, expiredValues, expiredDisabled);

  const administrator = await editor('/console/rules/41', 'TENANT_ADMIN');
  administrator.responses.push({ status: 204 });
  await button(administrator).fire('click');
  assert.equal(administrator.writes.length, 1);
  const unsaved = await editor('/console/rules/new');
  assert.equal(button(unsaved), undefined);
  assert.equal(unsaved.nodes.get('#content').innerHTML.includes('id="rule-delete"'), false);
  for (const role of ['VIEWER', 'MEMBER']) {
    const readonly = await editor('/console/rules/41', role);
    assert.equal(button(readonly), undefined);
    assert.match(readonly.nodes.get('#content').innerHTML, /当前角色没有规则编辑权限/);
    assert.equal(readonly.writes.length, 0);
  }
}

run().catch(error => { console.error(error); process.exitCode = 1; });
