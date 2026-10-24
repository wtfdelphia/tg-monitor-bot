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
    console.error('真实浏览器验收超过 180 秒');
    process.exit(1);
  }, 180000);

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
        if (url.protocol === 'data:' && request.resourceType() === 'image' && url.href.startsWith('data:image/svg+xml;')) {
          void request.continue();
          return;
        }
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
    const redaction = await page.addStyleTag({ content: '#identity-intake,#qr{visibility:hidden!important}' });
    try {
      await page.screenshot({ path: path.join(screenshots, `${name}.png`), fullPage: true });
    } finally {
      await redaction.evaluate(element => element.remove());
    }
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
    await anonymous.waitForFunction(() => {
      const image = document.querySelector('#qr-image');
      return image && !image.hidden && image.complete && image.naturalWidth >= 256;
    });
    assert.match(await anonymous.$eval('#qr-code', element => element.textContent), /^确认码：[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{4}$/);
    assert.match(await anonymous.$eval('#qr-link', element => element.href), /^https:\/\/t\.me\/browser_local_bot\?start=login_/);
    assert.equal(await anonymous.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), true);
    assert.equal(requests.some(request => request.pathname === '/auth/qr/challenge'), true);
    await anonymous.goto(config.origin + '/login');
    console.log('登录：本地挑战 handler、二维码渲染、确认码及窄屏布局通过；未执行 Telegram 真机确认');

    const bootstrap = await pageFor(config.empty);
    await navigate(bootstrap, '/console/identities', '身份', '#bot-intake-form');
    assert.equal(await bootstrap.$$eval('[data-identity-edit]', elements => elements.length), 0);
    for (const route of ['/api/v1/identities', '/api/v1/targets', '/api/v1/keywords', '/api/v1/rules']) {
      const response = await bootstrap.evaluate(async route => {
        const result = await fetch(route);
        return { status: result.status, body: await result.json() };
      }, route);
      assert.equal(response.status, 200);
      assert.deepEqual(response.body.items, [], '空租户不能预置业务对象或看到其他租户的数据');
    }
    await fill(bootstrap, '#bot-intake-form [name=token]', '123456:' + 'A'.repeat(35));
    await Promise.all([
      bootstrap.waitForResponse(response => response.url().endsWith('/api/v1/identities/bot') && response.status() === 201),
      bootstrap.$eval('#bot-intake-form', form => form.requestSubmit()),
    ]);
    await bootstrap.waitForFunction(() => document.querySelector('#content table')?.textContent.includes('浏览器接入 Bot'));
    assert.equal(await bootstrap.$eval('#bot-intake-form [name=token]', input => input.value), '');
    await navigate(bootstrap, '/console/targets', '目标频道', '#target-form');
    const bootstrapIdentity = await bootstrap.$eval('#target-form select', select => [...select.options].find(option => option.textContent.includes('浏览器接入 Bot'))?.value);
    assert.ok(bootstrapIdentity);
    await fill(bootstrap, '#target-form [name=chat]', '-10088021');
    await fill(bootstrap, '#target-form [name=title]', '空租户目标');
    await bootstrap.select('#target-form [name=sender_identity_id]', bootstrapIdentity);
    await Promise.all([
      bootstrap.waitForResponse(response => response.url().endsWith('/api/v1/targets') && response.status() === 201),
      bootstrap.$eval('#target-form', form => form.requestSubmit()),
    ]);
    await bootstrap.waitForFunction(() => document.querySelector('tbody tr')?.textContent.includes('可发送基础文字'));
    assert.match(await bootstrap.$eval('tbody tr', row => row.textContent), /目标内容保护：开启/);
    const bootstrapTarget = await bootstrap.$eval('[data-target-toggle]', element => element.dataset.targetToggle);
    await navigate(bootstrap, '/console/keywords', '关键词', '#keyword-form');
    await fill(bootstrap, '#keyword-form [name=word]', '空租户验证词');
    await bootstrap.select('#keyword-form [name=match_type]', 'fuzzy');
    await Promise.all([
      bootstrap.waitForResponse(response => response.url().endsWith('/api/v1/keywords') && response.status() === 201),
      bootstrap.$eval('#keyword-form', form => form.requestSubmit()),
    ]);
    await bootstrap.waitForFunction(() => document.querySelector('#content table')?.textContent.includes('空租户验证词'));
    await navigate(bootstrap, '/console/rules/new', '新建规则', '#rule-form');
    const bootstrapKeyword = await bootstrap.$eval('[name=keyword_id]', select => [...select.options].find(option => option.textContent === '空租户验证词')?.value);
    assert.ok(bootstrapKeyword);
    await bootstrap.select('[name=keyword_id]', bootstrapKeyword);
    await bootstrap.select('[name=target_ref]', bootstrapTarget);
    await Promise.all([bootstrap.waitForNavigation(), bootstrap.click('#rule-form button:not([type])')]);
    await bootstrap.waitForSelector('[data-rule-select]');
    assert.equal(await bootstrap.$$eval('[data-rule-select]', elements => elements.length), 1);
    const bootstrapRule = await bootstrap.$eval('#rules-list a', anchor => anchor.getAttribute('href'));
    await navigate(bootstrap, bootstrapRule, '编辑规则', '#rule-form');
    await bootstrap.reload();
    await bootstrap.waitForSelector('#rule-form');
    assert.equal(await bootstrap.$eval('[name=keyword_id]', select => select.value), bootstrapKeyword);
    assert.equal(await bootstrap.$eval('[name=target_ref]', select => select.value), bootstrapTarget);
    await bootstrap.browserContext().close();
    console.log('空租户：无业务预置，经页面接入 Bot、创建目标并自检、创建关键词与规则，刷新后引用持久化通过');

    const writer = await pageFor(config.writer);
    await writer.setViewport({ width: 1280, height: 900 });
    await navigate(writer, '/console/identities', '身份', '#bot-intake-form');
    assert.equal(await writer.$eval('#bot-intake-form [name=token]', input => input.type), 'password');
    await fill(writer, '#bot-intake-form [name=token]', '123456:' + 'A'.repeat(35));
    await Promise.all([
      writer.waitForResponse(response => response.url().endsWith('/api/v1/identities/bot') && response.status() === 201),
      writer.$eval('#bot-intake-form', form => form.requestSubmit()),
    ]);
    await writer.waitForFunction(() => document.querySelector('#content table')?.textContent.includes('浏览器接入 Bot'));
    assert.equal(await writer.$eval('#bot-intake-form [name=token]', input => input.value), '');
    console.log('Bot 接入：真实 HTTP handler、本地 getMe 桩、提交清空和列表刷新通过');

    await writer.click('[data-identity-reauthorize]');
    assert.equal(await writer.$eval('#identity-qr-form [name=consent]', input => input.checked), false);
    await fill(writer, '#identity-qr-form [name=api_id]', '12345');
    await fill(writer, '#identity-qr-form [name=api_hash]', 'b'.repeat(32));
    await writer.click('#identity-qr-form [name=consent]');
    await writer.$eval('#identity-qr-form', form => form.requestSubmit());
    await writer.waitForFunction(() => {
      const image = document.querySelector('#identity-qr-image');
      return image && !image.hidden && image.complete && image.naturalWidth === 256;
    });
    assert.equal(await writer.$eval('#identity-qr-form [name=api_hash]', input => input.value), '');
    await writer.waitForFunction(() => document.querySelector('#identity-qr-image')?.naturalWidth === 257, { timeout: 35000 });
    const desktopIntake = await writer.evaluate(() => ({ viewport: window.innerWidth, width: document.documentElement.scrollWidth }));
    assert.ok(desktopIntake.width <= desktopIntake.viewport + 1);
    await writer.setViewport({ width: 320, height: 900 });
    const intakeDimensions = await writer.evaluate(() => ({ viewport: window.innerWidth, width: document.documentElement.scrollWidth }));
    assert.ok(intakeDimensions.width <= intakeDimensions.viewport + 1);
    await writer.waitForFunction(() => !document.querySelector('#identity-password-form').hidden, { timeout: 65000 });
    assert.equal(await writer.$eval('#identity-qr-image', image => image.hasAttribute('src')), false);
    await new Promise(resolve => setTimeout(resolve, 2100));
    await fill(writer, '#identity-password-form [name=password]', 'browser-wrong-password');
    await writer.$eval('#identity-password-form', form => form.requestSubmit());
    await writer.waitForFunction(() => document.querySelector('#identity-intake-status').textContent.includes('密码错误'));
    assert.equal(await writer.$eval('#identity-password-form [name=password]', input => input.value), '');
    await new Promise(resolve => setTimeout(resolve, 2100));
    await fill(writer, '#identity-password-form [name=password]', 'browser-right-password');
    await writer.$eval('#identity-password-form', form => form.requestSubmit());
    await writer.waitForFunction(() => !document.querySelector('#identity-intake-status').textContent && document.querySelector('#identity-password-form').hidden);
    await writer.setViewport({ width: 1280, height: 900 });
    console.log('账号接入页面：本地 HTTP 桩的 QR 轮换、2FA 重试、凭据清空与终态刷新通过；不是 Telegram 真机或 MTProto 落库验收');

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

    await navigate(writer, '/console/targets', '目标频道', '#target-form');
    const botIdentity = await writer.$eval('#target-form select', element => [...element.options].find(option => option.textContent.includes('浏览器接入 Bot'))?.value);
    assert.ok(botIdentity);
    for (const [chat, expected] of [['-10088011', '可发送基础文字'], ['-10088012', '不可发送基础文字'], ['-10088013', '自检失败：TELEGRAM_REJECTED']]) {
      await fill(writer, '#target-form [name=chat]', chat);
      await writer.select('#target-form [name=sender_identity_id]', botIdentity);
      await Promise.all([
        writer.waitForResponse(response => response.url().endsWith('/api/v1/targets') && response.request().method() === 'POST' && response.status() === 201),
        writer.click('#target-form button'),
      ]);
      await writer.waitForFunction((chat, expected) => [...document.querySelectorAll('tbody tr')].some(row => row.cells[0].textContent === chat && row.textContent.includes(expected)), {}, chat, expected);
    }
    const lastProbe = await writer.$eval('tbody tr:last-child [data-target-probe]', element => element.dataset.targetProbe);
    await writer.focus('[data-target-probe="'+lastProbe+'"]');
    await Promise.all([
      writer.waitForResponse(response => response.url().endsWith('/probe') && response.request().method() === 'POST' && response.status() === 202),
      writer.keyboard.press('Enter'),
    ]);
    await writer.waitForFunction(() => document.body.textContent.includes('自检失败：TELEGRAM_REJECTED'));
    assert.equal(await writer.evaluate(() => document.body.textContent.includes('browser-secret-error-sentinel')), false);
    console.log('目标自检：创建排队、只读 Bot 桩、可发送/无发帖权/拒绝、键盘重试及错误脱敏通过');

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
    await navigate(viewer, '/console/identities', '身份', '[data-identity-edit]');
    assert.equal(await viewer.$('#identity-intake'), null);
    await navigate(viewer, '/console/keywords', '关键词', '#keyword-form');
    await viewer.waitForFunction(() => getComputedStyle(document.querySelector('#keyword-form')).display === 'none');
    assert.equal(await viewer.$eval('#keyword-form', form => getComputedStyle(form).display), 'none');
    await navigate(viewer, '/console/rules', '规则列表', '#rules-filter-form');
    assert.equal(await viewer.$('[data-rule-select]'), null);
    await navigate(viewer, '/console/rules/new', '新建规则', '#content .empty');
    assert.match(await viewer.$eval('#content', element => element.textContent), /没有规则编辑权限/);
    await navigate(viewer, '/console/targets', '目标频道', '#target-form');
    assert.equal(await viewer.$('[data-target-probe]'), null);
    assert.match(await viewer.$eval('#content', element => element.textContent), /不可发送基础文字/);
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
