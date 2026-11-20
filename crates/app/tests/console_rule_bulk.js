const assert = require('node:assert/strict');
const { browser, element } = require('./console_writes');

function deferred() {
  let resolve;
  const promise = new Promise(callback => { resolve = callback; });
  return { promise, resolve };
}

async function listPage(role = 'TENANT_OWNER', initial = null) {
  return browser('/console/rules', role, state => {
    const inputs = [];
    state.groups.set('[data-rule-select]', inputs);
    function renderRows(html, append = false) {
      if (!append) inputs.length = 0;
      for (const match of html.matchAll(/data-rule-select="([^"]+)" data-enabled="([^"]+)"/g)) {
        const input = element('input', { ruleSelect: match[1], enabled: match[2] });
        input.checked = false;
        const status = element('span');
        status.textContent = match[2] === 'true' ? '启用' : '停用';
        input.closest = () => ({ querySelector: () => status });
        input.status = status;
        inputs.push(input);
      }
    }
    const tbody = element('tbody');
    tbody.insertAdjacentHTML = (position, html) => { tbody.innerHTML += html; renderRows(html, true); };
    const pagination = element('div');
    let paginationHtml = '';
    Object.defineProperty(pagination, 'innerHTML', {
      get: () => paginationHtml,
      set: html => {
        paginationHtml = html;
        if (html.includes('id="rules-load-more"')) state.nodes.set('#rules-load-more', element());
        else state.nodes.delete('#rules-load-more');
      },
    });
    const content = state.nodes.get('#content');
    let contentHtml = '';
    Object.defineProperty(content, 'innerHTML', {
      get: () => contentHtml,
      set: html => {
        contentHtml = html;
        renderRows(html);
        if (html.includes('<tbody>')) state.nodes.set('#rules-list tbody', tbody);
        else state.nodes.delete('#rules-list tbody');
        tbody.innerHTML = html;
        state.nodes.set('#rules-pagination', pagination);
        const filters = element('form');
        const decode = value => value.replace(/&(amp|lt|gt|quot|#39);/g, entity => ({ '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"', '&#39;': "'" }[entity]));
        filters.elements = ['enabled', 'target_ref', 'source_ref'].map(name => ({
          name, disabled: false,
          value: name === 'enabled' ? html.match(/<option value="(true|false)" selected>/)?.[1] || '' : decode(html.match(new RegExp(`name="${name}"[^>]*value="([^"]*)"`))?.[1] || ''),
        }));
        const clear = element();
        filters.elements.push(element(), clear);
        state.nodes.set('#rules-filter-form', filters);
        state.nodes.set('#rules-filter-clear', clear);
        for (const selector of ['#rules-select-all', '#rules-selection-count', '#rules-bulk-message', '#rules-bulk-form']) state.nodes.delete(selector);
        if (html.includes('id="rules-bulk-form"')) {
          const all = element('input');
          all.checked = false;
          const form = element('form');
          const enable = element();
          const disable = element();
          enable.value = 'true';
          disable.value = 'false';
          form.elements = [enable, disable];
          state.nodes.set('#rules-select-all', all);
          state.nodes.set('#rules-selection-count', element('span'));
          state.nodes.set('#rules-bulk-message', element('p'));
          state.nodes.set('#rules-bulk-form', form);
        }
      },
    });
    content.insertAdjacentHTML = (position, html) => { contentHtml = html + contentHtml; };
    if (initial) state.readResponses.push({ status: 200, body: { user_id: 7100, role } }, initial);
  });
}

async function selectAll(state) {
  const all = state.nodes.get('#rules-select-all');
  all.checked = true;
  await all.fire('change');
}

function form(state) { return state.nodes.get('#rules-bulk-form'); }
function inputs(state) { return state.groups.get('[data-rule-select]'); }
function message(state) { return state.nodes.get('#rules-bulk-message'); }
function submit(state, enabled) { return form(state).fire('submit', { submitter: form(state).elements[enabled ? 0 : 1] }); }
function assertUnlocked(state) {
  assert.equal(form(state).dataset.pending, undefined);
  assert.ok(form(state).elements.every(button => !button.disabled));
  assert.ok(inputs(state).every(input => !input.disabled));
  assert.equal(state.nodes.get('#rules-select-all').disabled, false);
}

async function run() {
  const state = await listPage();
  assert.equal(inputs(state).length, 2);
  assert.match(state.nodes.get('#rules-selection-count').textContent, /已选 0 \/ 2/);
  await submit(state, false);
  assert.equal(state.writes.length, 0);
  assert.match(message(state).textContent, /请先选择/);
  inputs(state)[0].checked = true;
  await inputs(state)[0].fire('change');
  assert.equal(state.nodes.get('#rules-select-all').indeterminate, true);
  assert.match(state.nodes.get('#rules-selection-count').textContent, /已选 1 \/ 2/);
  await selectAll(state);
  state.window.confirm = () => false;
  await submit(state, false);
  assert.equal(state.writes.length, 0);
  assert.ok(inputs(state).every(input => input.checked));
  state.window.confirm = () => true;

  const first = deferred();
  const second = deferred();
  state.responses.push(first.promise, second.promise);
  const stopping = submit(state, false);
  assert.equal(state.writes.length, 1);
  assert.equal(form(state).dataset.pending, 'true');
  assert.ok(form(state).elements.every(button => button.disabled));
  assert.ok(inputs(state).every(input => input.disabled));
  assert.equal(state.nodes.get('#rules-load-more').disabled, true);
  const readsBefore = state.reads.length;
  await submit(state, true);
  await state.nodes.get('#rules-load-more').fire('click');
  assert.equal(state.writes.length, 1);
  assert.equal(state.reads.length, readsBefore);
  first.resolve({ status: 200, body: {} });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(state.writes.length, 2);
  assert.equal(inputs(state)[0].checked, false);
  assert.equal(inputs(state)[0].status.textContent, '停用');
  assert.equal(inputs(state)[1].checked, true);
  second.resolve({ status: 409, body: { error: { message: '<conflict>' } } });
  await stopping;
  assertUnlocked(state);
  assert.equal(state.nodes.get('#rules-load-more').disabled, false);
  assert.match(message(state).textContent, /已确认完成 1 \/ 2/);
  assert.match(message(state).textContent, /刷新页面核对/);
  assert.match(message(state).textContent, /<conflict>/);
  assert.equal(message(state).innerHTML, '');
  assert.equal(inputs(state)[1].checked, true);
  assert.match(state.nodes.get('#rules-selection-count').textContent, /已选 1 \/ 2/);
  assert.equal(state.window.location.href, '');
  for (const [index, request] of state.writes.entries()) {
    assert.equal(request.url, `/api/v1/rules/${41 + index}`);
    assert.equal(request.options.method, 'PATCH');
    assert.equal(request.options.credentials, 'same-origin');
    assert.equal(request.options.headers['x-csrf-token'], 'csrf-token');
    assert.deepEqual(JSON.parse(request.options.body), { enabled: false });
    assert.equal(request.url.includes('tenant'), false);
  }
  state.responses.push({ status: 200, body: {} });
  await submit(state, false);
  assert.equal(state.writes.length, 3);
  assert.equal(state.writes[2].url, '/api/v1/rules/42');
  assert.match(message(state).textContent, /已完成 1 条规则的停用/);
  assert.ok(inputs(state).every(input => !input.checked));
  assertUnlocked(state);

  const loading = deferred();
  state.readResponses.push(loading.promise);
  const more = state.nodes.get('#rules-load-more').fire('click');
  const loadingReads = state.reads.length;
  await state.nodes.get('#rules-load-more').fire('click');
  assert.equal(state.reads.length, loadingReads);
  await selectAll(state);
  await submit(state, true);
  assert.equal(state.writes.length, 3);
  assert.match(message(state).textContent, /列表正在加载/);
  loading.resolve({ status: 200, body: { items: [{ id: 43, keyword: '<img src=x>', target_tg_chat_id: -1001, action: 'COPY', priority: 2, enabled: false }], page: 2, has_more: false } });
  await more;
  assert.equal(inputs(state).length, 3);
  assert.equal(inputs(state)[0].checked, true);
  assert.equal(inputs(state)[2].checked, false);
  assert.equal(state.nodes.get('#rules-select-all').indeterminate, true);
  assert.match(state.nodes.get('#rules-list tbody').innerHTML, /&lt;img src=x&gt;/);
  assert.equal(state.nodes.has('#rules-load-more'), false);
  await selectAll(state);
  state.responses.push(...[41, 42, 43].map(() => ({ status: 200, body: {} })));
  await submit(state, true);
  assert.ok(inputs(state).every(input => !input.checked && input.status.textContent === '启用'));
  assert.match(message(state).textContent, /已完成 3 条规则的启用/);
  assertUnlocked(state);

  for (const response of [
    { status: 403, body: { error: { message: 'forbidden' } } },
    { status: 500, body: { error: { message: 'unavailable' } } },
    { error: new Error('network') },
    { status: 200, jsonError: new Error('invalid JSON') },
  ]) {
    const failed = await listPage();
    await selectAll(failed);
    failed.responses.push(response);
    await submit(failed, true);
    assert.equal(failed.writes.length, 1);
    assert.ok(inputs(failed).every(input => input.checked));
    assert.match(message(failed).textContent, /已确认完成 0 \/ 2/);
    assertUnlocked(failed);
  }
  const expired = await listPage();
  await selectAll(expired);
  expired.responses.push({ status: 401, body: {} });
  await submit(expired, true);
  assert.equal(expired.writes.length, 1);
  assert.equal(expired.window.location.href, '/login');
  assertUnlocked(expired);

  const admin = await listPage('TENANT_ADMIN');
  await selectAll(admin);
  admin.responses.push({ status: 200, body: {} }, { status: 200, body: {} });
  await submit(admin, true);
  assert.equal(admin.writes.length, 2);
  await form(admin).fire('submit');
  assert.equal(admin.writes.length, 2);

  const failedPage = await listPage();
  failedPage.readResponses.push({ status: 500, body: { error: { message: 'page unavailable' } } });
  await failedPage.nodes.get('#rules-load-more').fire('click');
  assert.equal(failedPage.nodes.get('#rules-load-more').disabled, false);
  assert.equal(inputs(failedPage).length, 2);
  assert.match(failedPage.nodes.get('#content').innerHTML, /page unavailable/);
  await failedPage.nodes.get('#rules-load-more').fire('click');
  assert.equal(inputs(failedPage).length, 3);

  const stopped = await listPage();
  await stopped.nodes.get('#rules-load-more').fire('click');
  await selectAll(stopped);
  stopped.responses.push(
    { status: 200, body: {} },
    { status: 409, body: { error: { message: 'conflict' } } },
  );
  await submit(stopped, false);
  assert.equal(stopped.writes.length, 2);
  assert.equal(inputs(stopped)[0].checked, false);
  assert.equal(inputs(stopped)[1].checked, true);
  assert.equal(inputs(stopped)[2].checked, true);
  assert.match(message(stopped).textContent, /已确认完成 1 \/ 3/);
  assertUnlocked(stopped);

  for (const role of ['VIEWER', 'MEMBER']) {
    const readonly = await listPage(role);
    assert.equal(readonly.nodes.has('#rules-bulk-form'), false);
    assert.equal(inputs(readonly).length, 0);
    assert.equal(readonly.nodes.get('#content').innerHTML.includes('href="/console/rules/new"'), false);
    assert.equal(readonly.nodes.get('#content').innerHTML.includes('>编辑</a>'), false);
    assert.equal(readonly.writes.length, 0);
  }
  const empty = await listPage('TENANT_OWNER', { status: 200, body: { items: [], page: 1, has_more: false } });
  await selectAll(empty);
  assert.equal(empty.nodes.get('#rules-select-all').checked, false);
  await submit(empty, true);
  assert.equal(empty.writes.length, 0);
  assert.match(empty.nodes.get('#content').innerHTML, /暂无规则/);
  assert.match(message(empty).textContent, /请先选择/);
}

if (require.main === module) run().catch(error => { console.error(error); process.exitCode = 1; });

module.exports = { listPage };
