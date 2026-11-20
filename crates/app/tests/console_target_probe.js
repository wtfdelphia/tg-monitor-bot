'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(0, 'utf8');

function harness(script = source, writable = true) {
  const label = { textContent: '' };
  const cell = { html: '', insertAdjacentHTML(position, markup) { this.html += markup; } };
  const button = { disabled: false, dataset: { targetProbe: '7' }, matches: () => false, addEventListener(name, handler) { this[name] = handler; } };
  let now = 0, timerId = 0, refreshed = 0;
  const timers = new Map(), events = new Map(), responses = [], calls = [];
  const context = {
    AbortController, AbortSignal, encodeURIComponent,
    Date: { now: () => now },
    window: { addEventListener: (name, handler) => events.set(name, handler), removeEventListener: name => events.delete(name) },
    document: {
      querySelector: selector => selector.startsWith('[data-target-toggle=') ? { closest: () => cell } : selector === '#target-probe-7' ? label : null,
      querySelectorAll: () => writable ? [button] : [],
    },
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; },
    clearTimeout: id => timers.delete(id),
    showError: () => { throw new Error('unexpected error'); },
    api: async (url, options) => { calls.push({ url, options }); return await responses.shift(); },
    writeApi: async () => { calls.push({ write: true }); return await responses.shift(); },
    targets: async () => { refreshed += 1; },
    escapeHtml: value => String(value ?? '').replaceAll('<', '&lt;'),
  };
  vm.createContext(context);
  const runWrite = script.slice(script.indexOf('async function runWrite('), script.indexOf('async function loadMe('));
  const definitions = script.slice(script.indexOf('function probeText('), script.indexOf('async function rules('));
  vm.runInContext(`let canWrite=${writable};let targetPageGeneration=1;let targetProbeCleanup=()=>{};${runWrite}\n${definitions}`, context);
  vm.runInContext('bindTargetProbes([{id:7,probe:{generation:2,state:"PENDING"}}],1)', context);
  return {
    context, label, cell, button, timers, events, calls, responses,
    refreshed: () => refreshed,
    async tick(delay) { const entry = [...timers].find(([, value]) => value.delay === delay); assert.ok(entry, `missing timer ${delay}`); timers.delete(entry[0]); now += delay; await entry[1].callback(); },
    stop() { vm.runInContext('targetProbeCleanup()', context); },
  };
}

async function checks(script = source) {
  {
    const state = harness(script);
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const pending = state.tick(2000);
    assert.equal(state.calls.length, 1);
    assert.equal([...state.timers.values()].filter(timer => timer.delay === 2000).length, 0);
    assert.equal(state.calls[0].options.cache, 'no-store');
    state.events.get('pagehide')();
    resolve({ generation: 2, state: 'SUCCEEDED', can_send: true });
    await pending;
    assert.equal(state.label.textContent, '');
    assert.equal(state.timers.size, 0);
    assert.equal(state.calls[0].options.signal.aborted, true);
  }
  {
    const state = harness(script);
    state.responses.push({ generation: 1, state: 'SUCCEEDED', can_send: true });
    await state.tick(2000);
    assert.equal(state.label.textContent, '');
    state.responses.push({ generation: 2, state: 'SUCCEEDED', can_send: false, content_protected: true, bot_role: 'administrator', checked_at: 'now' });
    await state.tick(2000);
    assert.match(state.label.textContent, /不可发送基础文字/);
    assert.match(state.label.textContent, /目标内容保护：开启/);
    assert.equal(state.timers.size, 0);
    state.responses.push({ state: 'PENDING' });
    await state.button.click();
    assert.equal(state.refreshed(), 1);
    state.events.get('pagehide')();
    state.responses.push({ state: 'PENDING' });
    await state.button.click();
    assert.equal(state.refreshed(), 1);
  }
  {
    const state = harness(script);
    await state.tick(60000);
    assert.match(state.label.textContent, /后台继续/);
    assert.equal(state.timers.size, 0);
  }
  {
    const state = harness(script);
    let resolve;
    state.responses.push(new Promise(callback => { resolve = callback; }));
    const pending = state.button.click();
    await state.button.click();
    assert.equal(state.calls.length, 1);
    resolve({ state: 'PENDING' });
    await pending;
    assert.equal(state.refreshed(), 1);
    assert.equal(state.button.disabled, false);
    state.stop();
  }
  {
    const state = harness(script, false);
    assert.equal(state.cell.html.includes('data-target-probe='), false);
    state.stop();
  }
}

(async () => {
  await checks();
  const broken = source.replace('if(!active())return;\n        if(probe.generation', 'if(probe.generation');
  assert.notEqual(broken, source);
  await assert.rejects(checks(broken));
  const noLock = source.replace("if(control.dataset.pending)return;", '');
  assert.notEqual(noLock, source);
  await assert.rejects(checks(noLock));
  console.log('目标自检：串行读取、离页取消、旧代次拒绝、等待上限、重复提交及反证通过');
})().catch(error => { console.error(error); process.exitCode = 1; });
