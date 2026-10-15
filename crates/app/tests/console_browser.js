'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const puppeteer = require('puppeteer');

async function main() {
  const config = JSON.parse(fs.readFileSync(0, 'utf8'));
  const origin = new URL(config.origin);
  assert.equal(origin.hostname, '127.0.0.1');
  assert.equal(origin.protocol, 'http:');
  const browser = await puppeteer.launch({
    executablePath: process.env.TGM_BROWSER_EXECUTABLE || puppeteer.executablePath(),
    headless: true,
    args: process.env.TGM_BROWSER_ALLOW_NO_SANDBOX === '1' ? ['--no-sandbox'] : [],
  });
  const failures = [];
  const requests = [];
  const screenshots = process.env.TGM_BROWSER_ARTIFACT_DIR;
  let heldRequest;
  let delayKeyword = false;
  const watchdog = setTimeout(() => {
    console.error('真实浏览器验收超过 120 秒');
    process.exit(1);
  }, 120000);

  function checkParameters(value) {
    if (!value || typeof value !== 'object') return;
    for (const [name, item] of Object.entries(value)) {
      assert.notEqual(name, 'tenant_id', '浏览器请求不能控制租户');
      checkParameters(item);
    }
  }

  async function pageFor(credentials) {
    const context = await browser.createBrowserContext();
    if (credentials) {
      await context.setCookie(
        { name: 'tgm_session', value: credentials.session, url: config.origin, httpOnly: true, sameSite: 'Lax' },
        { name: 'tgm_csrf', value: credentials.csrf, url: config.origin, sameSite: 'Lax' },
      );
    }
    const page = await context.newPage();
    page.setDefaultTimeout(10000);
    page.on('pageerror', error => failures.push(error.message));
    await page.setRequestInterception(true);
    page.on('request', request => {
      try {
        const url = new URL(request.url());
        assert.equal(url.origin, origin.origin, '测试不得向真实外部平台发送请求');
        assert.equal(url.searchParams.has('tenant_id'), false);
        if (request.postData()) checkParameters(JSON.parse(request.postData()));
        requests.push({ method: request.method(), pathname: url.pathname });
        if (delayKeyword && request.method() === 'POST' && url.pathname === '/api/v1/keywords') {
          heldRequest = request;
          delayKeyword = false;
        } else {
          void request.continue();
        }
      } catch (error) {
        failures.push(error.message);
        void request.abort();
      }
    });
    return page;
  }

  async function screenshot(page, name) {
    if (!screenshots) return;
    fs.mkdirSync(screenshots, { recursive: true });
    await page.screenshot({ path: path.join(screenshots, `${name}.png`), fullPage: true });
  }

  async function navigate(page, route, title, selector) {
    const response = await page.goto(config.origin + route);
    assert.equal(response.status(), 200, route);
    await page.waitForFunction(expected => document.querySelector('#title')?.textContent === expected, {}, title);
    await page.waitForSelector(selector);
    assert.equal(await page.$eval('#me', element => element.textContent.includes('正在读取')), false);
    assert.equal(await page.$eval('#content', element => element.textContent.includes('OTHER_TENANT_BROWSER_SENTINEL')), false);
  }

  async function fill(page, selector, value) {
    await page.focus(selector);
    await page.$eval(selector, element => { element.value = ''; });
    await page.type(selector, value);
  }

  async function layout(page, name) {
    const dimensions = await page.evaluate(() => ({
      viewport: window.innerWidth,
      document: document.documentElement.scrollWidth,
      main: document.querySelector('main').getBoundingClientRect().width,
    }));
    await screenshot(page, name);
    assert.ok(dimensions.document <= dimensions.viewport + 1, `${name} 页面横向溢出：${JSON.stringify(dimensions)}`);
    assert.ok(dimensions.main >= 180, `${name} 主内容宽度不可用`);
  }

  try {
    console.log(`真实浏览器：${await browser.version()}`);
    const anonymous = await pageFor();
    await anonymous.setViewport({ width: 390, height: 844 });
    const login = await anonymous.goto(config.origin + '/login');
    assert.equal(login.status(), 200);
    await layout(anonymous, 'login-mobile');
    const brokenLoginStyle = await anonymous.addStyleTag({ content: '.card{box-sizing:content-box}' });
    await assert.rejects(layout(anonymous, 'login-box-model-counterexample'), /页面横向溢出/);
    await brokenLoginStyle.evaluate(element => element.remove());
    await layout(anonymous, 'login-mobile-restored');
    await anonymous.keyboard.press('Tab');
    assert.equal(await anonymous.evaluate(() => document.activeElement.getAttribute('href')), '/auth/oidc/start');
    await anonymous.keyboard.press('Tab');
    assert.equal(await anonymous.evaluate(() => document.activeElement.id), 'qr-start');
    await anonymous.keyboard.press('Enter');
    await anonymous.waitForFunction(() => !document.querySelector('#qr-start').disabled && document.querySelector('#qr-state').textContent.includes('无法创建登录挑战'));
    assert.equal(await anonymous.$eval('#qr-link', element => element.hidden), true);
    console.log('登录：窄屏、键盘可达、未配置 Bot 时失败恢复通过；不代表真实扫码通过');

    const writer = await pageFor(config.writer);
    await writer.setViewport({ width: 1280, height: 900 });
    await navigate(writer, '/console/keywords', '关键词', '#keyword-form');
    assert.equal(await writer.evaluate(() => Boolean(window.browserInjected)), false);
    assert.equal(await writer.$('#content img'), null);
    await fill(writer, '#keyword-form [name=word]', '[');
    await writer.select('#keyword-form [name=match_type]', 'regex');
    await writer.focus('#keyword-form [name=word]');
    await writer.keyboard.press('Enter');
    await writer.waitForSelector('#content .danger');
    assert.equal(await writer.$eval('#keyword-form [name=word]', element => element.value), '[');
    assert.equal(await writer.$eval('#keyword-form button', element => element.disabled), false);

    await fill(writer, '#keyword-form [name=word]', '浏览器验证词');
    await writer.select('#keyword-form [name=match_type]', 'fuzzy');
    delayKeyword = true;
    const countBefore = requests.filter(item => item.method === 'POST' && item.pathname === '/api/v1/keywords').length;
    await writer.focus('#keyword-form [name=word]');
    await writer.keyboard.press('Enter');
    await writer.waitForFunction(() => document.querySelector('#keyword-form').dataset.pending === 'true');
    await new Promise(resolve => {
      const check = () => heldRequest ? resolve() : setTimeout(check, 10);
      check();
    });
    assert.ok(heldRequest, '应暂停真实请求以验证写入互斥');
    assert.equal(await writer.$eval('#keyword-form', form => [...form.elements].every(element => element.disabled)), true);
    await writer.$eval('#keyword-form', form => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })));
    const keywordResponse = writer.waitForResponse(response => response.url().endsWith('/api/v1/keywords') && response.request().method() === 'POST');
    await heldRequest.continue();
    heldRequest = undefined;
    assert.equal((await keywordResponse).status(), 201);
    await writer.waitForFunction(() => document.querySelector('#content table')?.textContent.includes('浏览器验证词'));
    assert.equal(requests.filter(item => item.method === 'POST' && item.pathname === '/api/v1/keywords').length, countBefore + 1);
    await writer.reload();
    await writer.waitForFunction(() => document.querySelector('#content table')?.textContent.includes('浏览器验证词'));
    console.log('关键词：真实错误恢复、输入保留、防重复提交、持久化及转义通过');

    await navigate(writer, '/console/rules/new', '新建规则', '#rule-form');
    const keyword = await writer.$eval('[name=keyword_id]', select => [...select.options].find(option => option.textContent === '浏览器验证词').value);
    await writer.select('[name=keyword_id]', keyword);
    await writer.click('#add-condition');
    await writer.type('[name=condition_value]', '排除词');
    await writer.click('#add-replacement');
    await writer.type('[name=replacement_pattern]', '浏览器');
    await writer.type('[name=replacement_value]', '真实浏览器');
    await writer.focus('#rule-form button:not([type])');
    await Promise.all([writer.waitForNavigation(), writer.keyboard.press('Enter')]);
    await writer.waitForSelector('[data-rule-select]');
    const ruleRoute = await writer.$eval('#rules-list a', anchor => anchor.getAttribute('href'));
    await writer.click('[data-rule-select]');
    writer.once('dialog', dialog => void dialog.accept());
    await writer.click('#rules-bulk-form button[value="false"]');
    await writer.waitForFunction(() => document.querySelector('[data-rule-select]')?.dataset.enabled === 'false');
    await navigate(writer, ruleRoute, '编辑规则', '#rule-form');
    assert.equal(await writer.$eval('[name=condition_value]', element => element.value), '排除词');
    assert.equal(await writer.$eval('[name=replacement_value]', element => element.value), '真实浏览器');
    await fill(writer, '[name=template]', '模板 <script>window.browserInjected=true</script>');
    await Promise.all([writer.waitForNavigation(), writer.click('#rule-form button:not([type])')]);
    await navigate(writer, ruleRoute, '编辑规则', '#rule-form');
    await writer.click('#rule-history-refresh');
    await writer.waitForSelector('[data-rule-version]');
    await writer.click('[data-rule-version]');
    await writer.waitForFunction(() => !document.querySelector('#rule-history-snapshot').hidden);
    assert.match(await writer.$eval('#rule-history-snapshot', element => element.textContent), /模板 <script>/);
    assert.equal(await writer.evaluate(() => Boolean(window.browserInjected)), false);
    console.log('规则：创建、批量停用、子项编辑、版本快照及转义通过');

    const routes = [
      ['/console/identities', '身份', '[data-identity-edit]'],
      ['/console/rules', '规则列表', '[data-rule-select]'],
      ['/console/rules/new', '新建规则', '#rule-form'],
      [ruleRoute, '编辑规则', '#rule-form'],
      ['/console/keywords', '关键词', '#keyword-form'],
      ['/console/targets', '目标频道', '#target-form'],
    ];
    for (const width of [1280, 768, 390, 320]) {
      await writer.setViewport({ width, height: 900 });
      for (const [route, title, selector] of routes) {
        await navigate(writer, route, title, selector);
        await layout(writer, `${title}-${width}`);
        await writer.focus('.nav a');
        await writer.keyboard.press('Tab');
        assert.equal(await writer.evaluate(() => document.activeElement.getAttribute('href')), '/console/rules');
      }
    }
    await navigate(writer, '/console/identities', '身份', '[data-identity-edit]');
    const brokenTableStyle = await writer.addStyleTag({ content: '.table-scroll{overflow-x:visible}' });
    await assert.rejects(layout(writer, 'table-scroll-counterexample'), /页面横向溢出/);
    await brokenTableStyle.evaluate(element => element.remove());
    await layout(writer, 'table-scroll-restored');
    await writer.focus('#content .table-scroll');
    await writer.keyboard.press('ArrowRight');
    await writer.waitForFunction(() => document.querySelector('#content .table-scroll').scrollLeft > 0);
    console.log('六页：桌面与窄屏布局、导航键盘焦点通过');

    const viewer = await pageFor(config.viewer);
    await navigate(viewer, '/console/keywords', '关键词', '#keyword-form');
    await viewer.waitForFunction(() => getComputedStyle(document.querySelector('#keyword-form')).display === 'none');
    assert.equal(await viewer.$eval('#keyword-form', form => getComputedStyle(form).display), 'none');
    await navigate(viewer, '/console/rules', '规则列表', '#rules-filter-form');
    assert.equal(await viewer.$('[data-rule-select]'), null);
    await navigate(viewer, '/console/rules/new', '新建规则', '#content .empty');
    assert.match(await viewer.$eval('#content', element => element.textContent), /没有规则编辑权限/);
    console.log('只读角色：业务写入入口隐藏、规则编辑拒绝通过');

    await writer.setViewport({ width: 390, height: 900 });
    await navigate(writer, '/console/identities', '身份', '[data-identity-edit]');
    await writer.click('#sessions-open');
    await writer.waitForSelector('[data-session-current="false"]');
    await layout(writer, 'sessions-mobile');
    writer.once('dialog', dialog => void dialog.accept());
    await writer.click('[data-session-current="false"]');
    await writer.waitForFunction(() => document.querySelectorAll('[data-session-revoke]').length === 1);
    writer.once('dialog', dialog => void dialog.accept());
    await Promise.all([writer.waitForNavigation(), writer.click('#logout')]);
    assert.equal(new URL(writer.url()).pathname, '/login');
    const protectedResponse = await writer.goto(config.origin + '/console/identities');
    assert.equal(protectedResponse.status(), 401);
    const expiredApi = await writer.evaluate(async () => (await fetch('/auth/me')).status);
    assert.equal(expiredApi, 401);
    assert.deepEqual(failures, []);
    console.log(`会话：远程撤销、退出登录及失效后拒绝通过；${requests.length} 个请求均无租户控制参数、无外部访问`);
  } catch (error) {
    for (const [index, page] of (await browser.pages()).entries()) {
      await screenshot(page, `failure-${index}`).catch(() => {});
    }
    throw error;
  } finally {
    if (heldRequest) await heldRequest.abort();
    clearTimeout(watchdog);
    await browser.close();
  }
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
