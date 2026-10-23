//! 控制台首发六页的轻量 SSR 壳。
//!
//! 页面不从 URL 或请求体读取 tenant_id，数据由同源 API 根据会话上下文返回。

use axum::{Router, response::Html, routing::get};

use crate::control::Authenticated;

pub fn route() -> Router<crate::control::ControlPlaneState> {
    Router::new()
        .route("/", get(login_page))
        .route("/login", get(login_page))
        .route("/console/identities", get(identities_page))
        .route("/console/rules", get(rules_page))
        .route("/console/rules/new", get(rule_editor_page))
        .route("/console/rules/{id}", get(rule_editor_page))
        .route("/console/keywords", get(keywords_page))
        .route("/console/targets", get(targets_page))
}

async fn login_page() -> Html<&'static str> {
    Html(LOGIN_PAGE)
}

async fn identities_page(Authenticated(_): Authenticated) -> Html<&'static str> {
    Html(CONSOLE_PAGE)
}

async fn rules_page(Authenticated(_): Authenticated) -> Html<&'static str> {
    Html(CONSOLE_PAGE)
}

async fn rule_editor_page(Authenticated(_): Authenticated) -> Html<&'static str> {
    Html(CONSOLE_PAGE)
}

async fn keywords_page(Authenticated(_): Authenticated) -> Html<&'static str> {
    Html(CONSOLE_PAGE)
}

async fn targets_page(Authenticated(_): Authenticated) -> Html<&'static str> {
    Html(CONSOLE_PAGE)
}

const LOGIN_PAGE: &str = r###"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>TGM 登录</title>
<style>
:root{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;color:#172033;background:#f5f7fb}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center}
.card{width:min(440px,calc(100% - 32px));background:#fff;border:1px solid #dfe5ef;border-radius:16px;padding:32px;box-shadow:0 14px 40px #17203314}
h1{margin:0 0 8px;font-size:28px}p{color:#5d687b;line-height:1.6}
button,a.button{display:block;width:100%;box-sizing:border-box;border:0;border-radius:10px;padding:12px 16px;margin-top:12px;font:inherit;text-align:center;text-decoration:none;cursor:pointer}
.primary{background:#2563eb;color:white}.secondary{background:#eef2ff;color:#1e40af}
#qr{margin-top:20px;padding:16px;background:#f8fafc;border-radius:10px;display:none}
.muted{font-size:13px;color:#687386;word-break:break-all}
</style>
</head>
<body>
<main class="card">
<h1>Telegram Monitor</h1>
<p>使用 Telegram 身份进入控制台。</p>
<a class="button primary" href="/auth/oidc/start">使用 Telegram 登录</a>
<button class="secondary" id="qr-start" type="button">扫码登录</button>
<section id="qr"><strong>这是控制台登录，不是 Telegram 账号接入。请比对网页与 Bot 的确认码后再确认。</strong><p id="qr-code"></p><a id="qr-link" class="muted" target="_blank" rel="noreferrer" hidden></a><p id="qr-state" class="muted" role="status" aria-live="polite"></p></section>
</main>
<script>
const start=document.querySelector('#qr-start');
const panel=document.querySelector('#qr');
const code=document.querySelector('#qr-code');
const link=document.querySelector('#qr-link');
const state=document.querySelector('#qr-state');
let timer;
let expiresAt;
function stopPolling(message){clearTimeout(timer);timer=undefined;state.textContent=message;start.disabled=false;link.hidden=true}
async function loginRequest(url,options={}){
  const response=await fetch(url,{...options,credentials:'same-origin',signal:AbortSignal.timeout(10000)});
  if(!response.ok){
    if(response.status===403){const body=await response.json();if(body.error?.code==='NOT_INVITED')throw new Error('你的 Telegram 账号已验证，但尚未被邀请加入任何工作区。请联系工作区管理员。')}
    throw new Error('请求失败，请重新开始。');
  }
  return response.json();
}
async function pollChallenge(challenge){
  timer=undefined;
  if(Date.now()>=expiresAt){stopPolling('登录挑战已过期，请重新开始。');return}
  try{
    const body=await loginRequest('/auth/qr/challenge/'+encodeURIComponent(challenge.nonce));
    if(body.state==='confirmed'){state.textContent='登录已确认，正在进入控制台…';window.location.href='/console/identities';return}
    if(body.state==='expired'){stopPolling('登录挑战已过期，请重新开始。');return}
    if(body.state==='rejected'){stopPolling('登录已被拒绝，请重新开始；若尚未受邀，请联系工作区管理员。');return}
    if(body.state!=='pending'&&body.state!=='awaiting_confirm')throw new Error('无法识别登录状态，请重新开始。');
    state.textContent=body.state==='pending'?'请在 Telegram 中打开链接，等待 Bot 确认卡片。':'请比对网页与 Bot 的确认码，仅在一致时确认登录。';
    timer=setTimeout(()=>pollChallenge(challenge),Math.max(0,Math.min(2000,expiresAt-Date.now())));
  }catch(error){stopPolling(error.message.startsWith('你的 Telegram')?error.message:'轮询失败，请重新开始。')}
}
start.addEventListener('click',async()=>{
  if(start.disabled)return;
  start.disabled=true;
  clearTimeout(timer);
  timer=undefined;
  panel.style.display='block';
  state.textContent='正在创建登录挑战…';
  code.textContent='';
  link.hidden=true;
  link.removeAttribute('href');
  link.textContent='';
  try{
    const challenge=await loginRequest('/auth/qr/challenge',{method:'POST',headers:{'content-type':'application/json'}});
    expiresAt=Date.parse(challenge.expires_at);
    const deepLink=new URL(challenge.deep_link);
    if(!/^[A-Za-z0-9_-]{43}$/.test(challenge.nonce)||!/^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{4}$/.test(challenge.confirm_code)||!Number.isFinite(expiresAt)||expiresAt<=Date.now()||deepLink.origin!=='https://t.me'||deepLink.username||deepLink.password||!/^\/[A-Za-z0-9_]+$/.test(deepLink.pathname)||deepLink.searchParams.get('start')!=='login_'+challenge.nonce)throw new Error('登录挑战无效，请重新开始。');
    code.textContent='确认码：'+challenge.confirm_code;
    link.href=deepLink.href;
    link.textContent=deepLink.href;
    link.hidden=false;
    state.textContent='请在 Telegram 中打开链接，并比对确认码。';
    timer=setTimeout(()=>pollChallenge(challenge),Math.min(2000,expiresAt-Date.now()));
  }catch(error){stopPolling(error.message.startsWith('你的 Telegram')?error.message:'无法创建登录挑战，请稍后重试。')}
});
</script>
</body>
</html>"###;

const CONSOLE_PAGE: &str = r###"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Telegram Monitor 控制台</title>
<style>
:root{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;color:#172033;background:#f5f7fb}
*{box-sizing:border-box}body{margin:0}.layout{display:flex;min-height:100vh}
.nav{width:220px;background:#172033;color:#dbe5ff;padding:24px 16px}.brand{font-size:20px;font-weight:700;margin:0 8px 24px}
.nav a{display:block;color:#cbd5e1;text-decoration:none;padding:10px 12px;border-radius:8px;margin:4px 0}.nav a:hover{background:#253454;color:#fff}
.main{flex:1;min-width:0;padding:32px;max-width:1280px}.top{display:flex;justify-content:space-between;gap:16px;align-items:start;flex-wrap:wrap}.top h1{margin:0 0 6px}
.muted{color:#687386}.card{background:#fff;border:1px solid #dfe5ef;border-radius:12px;padding:20px;margin-top:24px}
.toolbar{display:flex;gap:10px;flex-wrap:wrap;margin:16px 0}.toolbar form{margin:0}
button,input,select,textarea{font:inherit;border:1px solid #cbd5e1;border-radius:8px;padding:9px 10px;max-width:100%;min-width:0}
button{cursor:pointer;background:#2563eb;color:#fff;border-color:#2563eb}.secondary{background:#eef2ff;color:#1e40af;border-color:#c7d2fe}.danger-button{background:#fff1f2;color:#b91c1c;border-color:#fecdd3}
.table-scroll{max-width:100%;overflow-x:auto}table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:11px 8px;border-bottom:1px solid #eef2f7;vertical-align:top}th{font-size:13px;color:#687386}
form.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(220px,100%),1fr));gap:14px}label{display:grid;gap:6px;font-size:14px;color:#4b5563}
.full{grid-column:1/-1}.notice{padding:12px;border-radius:8px;background:#eff6ff;color:#1e40af}.danger{color:#b91c1c}.empty{color:#687386;padding:20px 0}
.actions{display:flex;gap:6px;flex-wrap:wrap}.actions button{padding:6px 9px;font-size:13px}.inline-form{display:grid;grid-template-columns:repeat(2,minmax(120px,1fr));gap:6px;margin-top:8px}
details summary{cursor:pointer;color:#1e40af}.status-ok{color:#15803d}.status-warn{color:#b45309}.status-off{color:#64748b}
pre{white-space:pre-wrap;overflow-wrap:anywhere}
@media(max-width:700px){.layout{flex-direction:column}.nav{width:100%;padding:16px}.brand{margin-bottom:8px}.nav a{display:inline-block;margin:0}.main{padding:16px}.card{padding:16px}.inline-form{grid-template-columns:1fr}}
</style>
</head>
<body>
<div class="layout">
<nav class="nav"><div class="brand">TGM Console</div><a href="/console/identities">身份</a><a href="/console/rules">规则列表</a><a href="/console/keywords">关键词</a><a href="/console/targets">目标频道</a></nav>
<main class="main"><div class="top"><div><h1 id="title">控制台</h1><div id="me" class="muted">正在读取当前身份…</div></div><div class="actions"><button id="sessions-open" class="secondary" type="button" disabled>会话管理</button><button id="logout" class="danger-button" type="button" disabled>退出登录</button></div></div><section id="sessions-panel" class="card" hidden></section><section id="content" class="card"><div class="empty">正在加载页面…</div></section></main>
</div>
<script>
const content=document.querySelector('#content');
const title=document.querySelector('#title');
const me=document.querySelector('#me');
const path=window.location.pathname;
const MAX_LIST_PAGES=100;
let keywordFilterState={q:'',match_type:'',category:''};
let ruleFilterState={enabled:'',target_ref:'',source_ref:''};
let ruleListLoading=false;
let currentRole='';
let canWrite=false;
let identityIntakeCleanup=()=>{};
const escapeHtml=value=>String(value??'').replace(/[&<>'"]/g,char=>({'&':'&amp;','<':'&lt;','>':'&gt;',"'":'&#39;','"':'&quot;'}[char]));
const getCsrf=()=>document.cookie.split(';').map(value=>value.trim()).find(value=>value.startsWith('tgm_csrf='))?.slice('tgm_csrf='.length)||'';
function showError(error){content.insertAdjacentHTML('afterbegin','<div class="danger">'+escapeHtml(error.message||'请求失败')+'</div>')}
async function api(url,options={}){
  const response=await fetch(url,{credentials:'same-origin',...options});
  if(response.status===401){window.location.href='/login';throw new Error('登录已失效')}
  const body=response.headers.get('content-type')?.includes('json')?await response.json():await response.text();
  if(!response.ok)throw new Error(body?.error?.message||'请求失败');
  return body;
}
async function apiAllPages(url){
  let body={items:[],has_more:false};
  const items=[];
  for(let page=1;page<=MAX_LIST_PAGES;page+=1){
    const separator=url.includes('?')?'&':'?';
    body=await api(url+separator+'page='+page);
    const pageItems=Array.isArray(body.items)?body.items:[];
    items.push(...pageItems);
    if(!body.has_more)break;
    if(pageItems.length===0)throw new Error('分页响应没有进展，请稍后重试');
    if(page===MAX_LIST_PAGES)throw new Error('列表超过控制台加载上限，请先使用筛选条件');
  }
  return {...body,items};
}
async function writeApi(url,method,body){
  if(!canWrite)throw new Error('当前角色没有写入权限');
  return api(url,{method,headers:{'content-type':'application/json','x-csrf-token':getCsrf()},body:body===undefined?undefined:JSON.stringify(body)});
}
async function writeRawApi(url,contentType,body){
  if(!canWrite)throw new Error('当前角色没有写入权限');
  return api(url,{method:'POST',headers:{'content-type':contentType,'x-csrf-token':getCsrf()},body});
}
async function runWrite(control,operation){
  if(control.dataset.pending)return;
  control.dataset.pending='true';
  const controls=control.matches('form')?[...control.elements]:[control];
  const previous=controls.map(element=>[element,element.disabled]);
  controls.forEach(element=>{element.disabled=true});
  try{await operation()}catch(error){showError(error)}finally{previous.forEach(([element,disabled])=>{element.disabled=disabled});delete control.dataset.pending}
}
async function loadMe(){try{const body=await api('/auth/me');currentRole=String(body.role||'');canWrite=currentRole==='TENANT_OWNER'||currentRole==='TENANT_ADMIN';me.textContent='用户 '+escapeHtml(body.user_id)+' · '+escapeHtml(currentRole);return true}catch(error){if(error.message==='登录已失效'){me.textContent=error.message;return false}me.textContent='控制台暂时不可用';showError(error);return false}}
async function sessionWriteApi(url,method){return api(url,{method,headers:{'x-csrf-token':getCsrf()}})}
function showSessionError(error){document.querySelector('#sessions-panel').insertAdjacentHTML('afterbegin','<div class="danger">'+escapeHtml(error.message||'会话操作失败')+'</div>')}
async function sessions(){
  const panel=document.querySelector('#sessions-panel');
  panel.hidden=false;
  const body=await api('/auth/sessions');
  const methods={oidc:'Telegram OIDC',bot_qr:'控制 Bot 扫码',unknown:'未知（历史会话）'};
  const rows=body.items.map(item=>'<tr><td>'+escapeHtml(item.ip||'未知')+'</td><td>'+escapeHtml(item.user_agent||'未知')+'</td><td>'+escapeHtml(methods[item.login_method]||'未知')+'</td><td>'+escapeHtml(item.created_at)+'</td><td>'+escapeHtml(item.expires_at)+'</td><td>'+(item.is_current?'当前会话':'其他会话')+'</td><td><button class="danger-button" type="button" data-session-revoke="'+escapeHtml(item.id)+'" data-session-current="'+String(item.is_current===true)+'">撤销</button></td></tr>');
  panel.innerHTML='<div class="top"><h2>本人活跃会话</h2><div class="actions"><button id="sessions-refresh" class="secondary" type="button">刷新</button><button id="sessions-close" class="secondary" type="button">收起</button></div></div><p class="muted">包含本人在各工作区的活跃会话；撤销当前会话会退出登录。</p>'+(rows.length?renderTable(['IP','浏览器 / 设备','登录方式','创建时间','过期时间','会话','操作'],rows):'<div class="empty">没有活跃会话。</div>');
  document.querySelector('#sessions-close').addEventListener('click',()=>{panel.hidden=true});
  document.querySelector('#sessions-refresh').addEventListener('click',async event=>{const button=event.currentTarget;button.disabled=true;try{await sessions()}catch(error){showSessionError(error)}finally{button.disabled=false}});
  document.querySelectorAll('[data-session-revoke]').forEach(button=>button.addEventListener('click',async()=>{
    const isCurrent=button.dataset.sessionCurrent==='true';
    if(!window.confirm(isCurrent?'撤销当前会话并退出登录？':'撤销这个会话并让该设备下线？'))return;
    button.disabled=true;
    try{await sessionWriteApi('/auth/sessions/'+encodeURIComponent(button.dataset.sessionRevoke),'DELETE');if(isCurrent){window.location.href='/login';return}await sessions()}catch(error){showSessionError(error)}finally{button.disabled=false}
  }));
}
function bindSessionControls(){
  const open=document.querySelector('#sessions-open');
  const logout=document.querySelector('#logout');
  open.disabled=false;
  logout.disabled=false;
  open.addEventListener('click',async()=>{open.disabled=true;try{await sessions()}catch(error){showSessionError(error)}finally{open.disabled=false}});
  logout.addEventListener('click',async()=>{if(!window.confirm('退出当前登录？'))return;logout.disabled=true;try{await sessionWriteApi('/auth/logout','POST');window.location.href='/login'}catch(error){showError(error)}finally{logout.disabled=false}});
}
function renderTable(headers,rows){return '<div class="table-scroll" tabindex="0" role="region" aria-label="可横向滚动的数据表"><table><thead><tr>'+headers.map(value=>'<th>'+value+'</th>').join('')+'</tr></thead><tbody>'+rows.join('')+'</tbody></table></div>'}
function identityStatus(status){const cls=status==='online'?'status-ok':status==='offline'?'status-off':'status-warn';return '<span class="'+cls+'">'+escapeHtml(status)+'</span>'}
function targetStatus(item){return item.enabled?'<span class="status-ok">启用</span>':'<span class="status-off">停用</span>'}
function readOnlyNotice(){return '<div id="console-readonly-notice" class="notice">当前角色为只读，仅可查看数据；写操作需要租户所有者或管理员。</div>'}
function enforceReadOnly(){if(canWrite)return;if(!document.querySelector('#console-readonly-notice'))content.insertAdjacentHTML('afterbegin',readOnlyNotice());['[data-identity-reset]','[data-identity-delete]','[data-identity-edit]','[data-keyword-toggle]','[data-keyword-delete]','[data-keyword-edit]','[data-target-toggle]','[data-target-delete]','[data-target-edit]'].forEach(selector=>document.querySelectorAll(selector).forEach(element=>{(element.closest('details')||element.closest('.actions')||element).style.display='none'}));['#keyword-form','#keyword-test','#keyword-import-label','#target-form','a[href="/console/rules/new"]'].forEach(selector=>document.querySelectorAll(selector).forEach(element=>{element.style.display='none'}));document.querySelectorAll('a[href^="/console/rules/"]').forEach(element=>{element.style.display='none'})}

async function identities(){
  identityIntakeCleanup();
  title.textContent='身份';
  const body=await apiAllPages('/api/v1/identities');
  const rows=body.items.map(item=>'<tr><td>'+escapeHtml(item.display_name||'未命名')+'</td><td>'+escapeHtml(item.kind)+'</td><td>'+escapeHtml(item.tg_user_id||'—')+'</td><td>'+identityStatus(item.status)+'</td><td><div class="actions"><button data-identity-reset="'+item.id+'" type="button">状态复位</button><button class="danger-button" data-identity-delete="'+item.id+'" type="button">删除</button></div><details><summary>编辑</summary><form class="inline-form" data-identity-edit="'+item.id+'"><input name="display_name" placeholder="显示名称" value="'+escapeHtml(item.display_name||'')+'"><input name="proxy_url" placeholder="填写才修改代理，留空不变"><label><input name="clear_proxy" type="checkbox">清除代理</label><button type="submit">保存</button></form></details></td></tr>');
  content.innerHTML='<div class="notice">身份用于读取 Telegram 频道。敏感凭据不在页面回显；状态复位、显示名称和代理配置沿用现有 API。</div>'+(canWrite?identityIntakeMarkup(body.items):'')+(rows.length?renderTable(['名称','类型','Telegram 用户','状态','操作'],rows):'<div class="empty">暂无身份。</div>');
  bindIdentityIntake();
  document.querySelectorAll('[data-identity-reset]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{await writeApi('/api/v1/identities/'+button.dataset.identityReset+'/reset-status','POST');await identities()})));
  document.querySelectorAll('[data-identity-delete]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{if(!window.confirm('确定删除这个身份吗？'))return;await writeApi('/api/v1/identities/'+button.dataset.identityDelete,'DELETE');await identities()})));
  document.querySelectorAll('[data-identity-edit]').forEach(form=>form.addEventListener('submit',async event=>{event.preventDefault();const data=new FormData(form);const displayName=String(data.get('display_name')||'').trim();const proxyUrl=String(data.get('proxy_url')||'').trim();const body={display_name:displayName||null};if(data.get('clear_proxy')==='on')body.proxy_url=null;else if(proxyUrl)body.proxy_url=proxyUrl;await runWrite(form,async()=>{await writeApi('/api/v1/identities/'+form.dataset.identityEdit,'PATCH',body);await identities()})}));
}

function identityIntakeMarkup(items){
  const reauthorize=items.filter(item=>item.kind==='mtproto').map(item=>'<button type="button" class="secondary" data-identity-reauthorize="'+escapeHtml(item.id)+'">重新授权：'+escapeHtml(item.display_name||item.tg_user_id||item.id)+'</button>').join('');
  return `<section id="identity-intake"><style>#identity-intake [hidden]{display:none!important}</style>
<h2>接入身份</h2>
<form id="bot-intake-form" class="toolbar"><label>Bot Token<input name="token" type="password" autocomplete="off" required></label><button>接入 Bot</button></form>
<p>Telegram 账号接入不是控制台登录，仅 pro 档可用。平台代持的 session 可读取账号会话、以账号发送消息及修改设置；加密不能消除代持风险。请仅接入有权使用的测试账号。</p>
<p>内测占位授权版本：custody-v0，非正式法律文本。本次授权平台代持 Telegram session，用于读取本人有权访问的频道；投递使用 Bot，不以该用户账号发送。</p>
<p id="identity-reauthorize-label">新接入 Telegram 账号</p><div class="actions">${reauthorize}</div>
<form id="identity-qr-form" class="toolbar"><label>API ID<input name="api_id" inputmode="numeric" pattern="[0-9]+" required></label><label>API Hash<input name="api_hash" type="password" autocomplete="off" required></label><label><input name="consent" type="checkbox" required>我已阅读上述权限与代持风险，同意本次按 custody-v0 内测占位版本授权</label><button>开始账号扫码</button></form>
<img id="identity-qr-image" alt="Telegram 账号接入二维码，包含短期授权凭据，请勿分享" width="256" height="256" style="max-width:100%;height:auto" hidden>
<form id="identity-password-form" class="toolbar" hidden><label>Telegram 两步验证密码<input name="password" type="password" autocomplete="off" required></label><button>提交两步验证密码</button></form>
<button id="identity-login-stop" class="secondary" type="button" hidden>结束本页操作</button><p class="muted">结束或离页会清除本页材料；服务端进行中的登录仍会在既有时限内过期，不表示已注销 Telegram 会话。</p>
<p id="identity-intake-status" role="status" aria-live="polite"></p></section>`;
}

function bindIdentityIntake(){
  const botForm=document.querySelector('#bot-intake-form');
  const qrForm=document.querySelector('#identity-qr-form');
  if(!botForm||!qrForm)return;
  const passwordForm=document.querySelector('#identity-password-form');
  const image=document.querySelector('#identity-qr-image');
  const status=document.querySelector('#identity-intake-status');
  const stop=document.querySelector('#identity-login-stop');
  let generation=0,loginId='',identityId='',timer,expiryTimer,imageTimer,nextRequestAt=0;
  let controller=new AbortController();
  const clearImage=()=>{clearTimeout(imageTimer);image.hidden=true;image.removeAttribute('src')};
  function reset(){
    generation+=1;controller.abort();controller=new AbortController();
    clearTimeout(timer);clearTimeout(expiryTimer);clearImage();loginId='';nextRequestAt=0;
    passwordForm.hidden=true;stop.hidden=true;
    for(const form of [botForm,qrForm,passwordForm])for(const input of form.elements)if(input.type==='password')input.value='';
  }
  const leave=()=>reset();
  window.addEventListener('pagehide',leave);
  identityIntakeCleanup=()=>{reset();window.removeEventListener('pagehide',leave)};
  async function intakeRequest(url,method,body,signal){
    if(!canWrite)throw new Error('当前角色不能接入身份。');
    let response;
    try{
      response=await fetch(url,{method,credentials:'same-origin',cache:'no-store',signal:AbortSignal.any([signal,AbortSignal.timeout(10000)]),headers:method==='POST'?{'content-type':'application/json','x-csrf-token':getCsrf()}:undefined,body:body===undefined?undefined:JSON.stringify(body)});
    }catch{throw new Error('接入请求未确认，请核对身份列表后重新开始。')}
    if(!response.ok){
      const messages={401:'登录已失效，请重新登录。',403:'无接入权限，请核对角色、pro 档及授权版本。',404:'登录或身份不可用，请重新开始。',429:'请求过于频繁，请稍后重新开始。'};
      throw new Error(messages[response.status]||'接入未完成，请核对输入与身份列表后重试。');
    }
    try{return await response.json()}catch{throw new Error('接入响应无效，请重新开始。')}
  }
  function showQr(svg,expiresAt){
    const expiry=Date.parse(expiresAt);
    if(typeof svg!=='string'||svg.length>524288||!Number.isFinite(expiry)||expiry<=Date.now())throw new Error('二维码已过期或无效，请重新开始。');
    const document=new DOMParser().parseFromString(svg,'image/svg+xml');
    if(document.querySelector('parsererror')||document.documentElement.localName!=='svg'||document.documentElement.namespaceURI!=='http://www.w3.org/2000/svg')throw new Error('二维码响应无效，请重新开始。');
    clearImage();image.src='data:image/svg+xml;charset=utf-8,'+encodeURIComponent(svg);image.hidden=false;
    imageTimer=setTimeout(clearImage,expiry-Date.now());
  }
  function finish(message){reset();status.textContent=message}
  async function update(body,current){
    if(current!==generation)return;
    if(body.status==='confirmed'){finish('账号已接入，正在刷新身份列表。');await identities();return}
    if(body.status==='expired'){finish('账号扫码已过期，请重新开始。');return}
    if(body.status==='failed'){
      const reasons={account_mismatch:'登录账号与原身份不一致，未重新授权。',account_already_registered:'该账号已接入，请核对身份列表。'};
      finish(reasons[body.reason]||'账号接入失败，请核对身份列表后重新开始。');return;
    }
    if(body.status==='awaiting_password'){
      clearImage();passwordForm.hidden=false;status.textContent=body.password_rejected?'两步验证密码错误，请重新输入。':'请填写 Telegram 两步验证密码。';return;
    }
    if(body.status!=='pending')throw new Error('无法识别账号登录状态，请重新开始。');
    passwordForm.hidden=true;showQr(body.qr_svg,body.expires_at);status.textContent='请使用 Telegram 扫描账号接入二维码；不要分享此二维码。';
    timer=setTimeout(async()=>{
      if(current!==generation)return;
      try{nextRequestAt=Date.now()+2000;const result=await intakeRequest('/api/v1/identities/login/qr/'+encodeURIComponent(loginId),'GET',undefined,controller.signal);if(current!==generation)return;nextRequestAt=Date.now()+2000;await update(result,current)}
      catch(error){if(current===generation)finish(error.message)}
    },25000);
  }
  botForm.addEventListener('submit',async event=>{
    event.preventDefault();if(botForm.dataset.pending)return;
    const input=botForm.elements.namedItem('token');const token=input.value;input.value='';
    const current=generation;
    await runWrite(botForm,async()=>{
      try{await intakeRequest('/api/v1/identities/bot','POST',{token},controller.signal);if(current===generation)await identities()}
      catch(error){if(current===generation)status.textContent=error.message}
    });
  });
  qrForm.addEventListener('submit',async event=>{
    event.preventDefault();if(qrForm.dataset.pending)return;
    const hash=qrForm.elements.namedItem('api_hash');const apiHash=hash.value;hash.value='';
    if(!qrForm.elements.namedItem('consent').checked){status.textContent='请先阅读并主动同意代持授权。';return}
    const apiId=Number(qrForm.elements.namedItem('api_id').value);
    if(!Number.isInteger(apiId)||apiId<=0||apiId>2147483647||!/^[a-fA-F0-9]{32}$/.test(apiHash)){status.textContent='请填写有效的 API ID 与 API Hash。';return}
    reset();const current=generation;stop.hidden=false;
    const body={api_id:apiId,api_hash:apiHash,consent_text_version:'custody-v0'};
    if(identityId)body.identity_id=Number(identityId);
    status.textContent='正在发起账号接入…';
    expiryTimer=setTimeout(()=>{if(current===generation)finish('本页登录已超时，请核对身份列表后重新开始。')},600000);
    await runWrite(qrForm,async()=>{
      try{
        const result=await intakeRequest('/api/v1/identities/login/qr','POST',body,controller.signal);
        if(current!==generation)return;
        if(!/^[A-Za-z0-9_-]{43}$/.test(result.login_id))throw new Error('登录材料无效，请重新开始。');
        loginId=result.login_id;nextRequestAt=Date.now()+2000;
        await update({status:'pending',qr_svg:result.qr_svg,expires_at:result.expires_at},current);
      }catch(error){if(current===generation)finish(error.message)}
    });
  });
  passwordForm.addEventListener('submit',async event=>{
    event.preventDefault();if(passwordForm.dataset.pending)return;
    const input=passwordForm.elements.namedItem('password');const password=input.value;input.value='';
    if(!loginId||passwordForm.hidden||!password)return;
    if(Date.now()<nextRequestAt){status.textContent='请稍候两秒再提交密码。';return}
    const current=generation;
    await runWrite(passwordForm,async()=>{
      try{const body=await intakeRequest('/api/v1/identities/login/password','POST',{login_id:loginId,password},controller.signal);if(current!==generation)return;nextRequestAt=Date.now()+2000;await update(body,current)}
      catch(error){if(current===generation)finish(error.message)}
    });
  });
  stop.addEventListener('click',()=>finish('本页操作已结束；服务端登录将按原时限过期。'));
  document.querySelectorAll('[data-identity-reauthorize]').forEach(button=>button.addEventListener('click',()=>{
    reset();identityId=button.dataset.identityReauthorize;qrForm.elements.namedItem('consent').checked=false;
    document.querySelector('#identity-reauthorize-label').textContent='重新授权已有身份 '+identityId+'，必须使用原 Telegram 账号；刷新本页可切回新接入。';
    status.textContent='请重新填写凭据并主动同意授权。';qrForm.elements.namedItem('api_id').focus();
  }));
}

async function keywords(){
  title.textContent='关键词';
  const filterParams=new URLSearchParams();
  if(keywordFilterState.q)filterParams.set('q',keywordFilterState.q);
  if(keywordFilterState.match_type)filterParams.set('match_type',keywordFilterState.match_type);
  if(keywordFilterState.category)filterParams.set('category',keywordFilterState.category);
  const filterQuery=filterParams.toString();
  const body=await apiAllPages('/api/v1/keywords'+(filterQuery?'?'+filterQuery:''));
  const rows=body.items.map(item=>'<tr><td>'+escapeHtml(item.word)+'</td><td>'+escapeHtml(item.match_type)+'</td><td>'+escapeHtml(item.category||'—')+'</td><td>'+item.priority+'</td><td>'+(item.enabled?'启用':'停用')+'</td><td><div class="actions"><button data-keyword-toggle="'+item.id+'" data-enabled="'+item.enabled+'" type="button">'+(item.enabled?'停用':'启用')+'</button><button class="danger-button" data-keyword-delete="'+item.id+'" type="button">删除</button></div><details><summary>编辑</summary><form class="inline-form" data-keyword-edit="'+item.id+'"><input name="word" required value="'+escapeHtml(item.word)+'"><select name="match_type"><option value="fuzzy" '+(item.match_type==='fuzzy'?'selected':'')+'>fuzzy</option><option value="exact" '+(item.match_type==='exact'?'selected':'')+'>exact</option><option value="regex" '+(item.match_type==='regex'?'selected':'')+'>regex</option></select><input name="category" placeholder="分类" value="'+escapeHtml(item.category||'')+'"><input name="priority" type="number" value="'+item.priority+'"><button type="submit">保存</button></form></details></td></tr>');
  content.innerHTML='<div class="notice">关键词用于规则触发。删除被规则引用的关键词时，服务端会返回引用关系，不会静默破坏规则。</div><div class="toolbar"><form id="keyword-filter-form" class="toolbar"><input name="q" placeholder="搜索关键词" value="'+escapeHtml(keywordFilterState.q)+'"><select name="match_type"><option value="">全部匹配方式</option><option value="fuzzy" '+(keywordFilterState.match_type==='fuzzy'?'selected':'')+'>fuzzy</option><option value="exact" '+(keywordFilterState.match_type==='exact'?'selected':'')+'>exact</option><option value="regex" '+(keywordFilterState.match_type==='regex'?'selected':'')+'>regex</option></select><input name="category" placeholder="按分类筛选" value="'+escapeHtml(keywordFilterState.category)+'"><button class="secondary">筛选</button><button id="keyword-filter-clear" class="secondary" type="button">清除筛选</button></form><form id="keyword-form" class="toolbar"><input name="word" placeholder="关键词" required><select name="match_type"><option value="fuzzy">fuzzy</option><option value="exact">exact</option><option value="regex">regex</option></select><input name="category" placeholder="分类"><input name="priority" type="number" value="0"><button>新增</button></form><button id="keyword-test" class="secondary">试跑</button><button id="keyword-export" class="secondary" type="button">导出 JSON</button><label id="keyword-import-label" class="button secondary">导入文件<input id="keyword-import" type="file" accept=".json,.csv,application/json,text/csv" hidden></label></div>'+(rows.length?renderTable(['词','匹配方式','分类','优先级','状态','操作'],rows):'<div class="empty">暂无关键词。</div>');
  document.querySelector('#keyword-filter-form').addEventListener('submit',event=>{event.preventDefault();const form=new FormData(event.currentTarget);keywordFilterState={q:String(form.get('q')||'').trim(),match_type:String(form.get('match_type')||''),category:String(form.get('category')||'').trim()};keywords().catch(showError)});
  document.querySelector('#keyword-filter-clear').addEventListener('click',()=>{keywordFilterState={q:'',match_type:'',category:''};keywords().catch(showError)});
  document.querySelector('#keyword-form').addEventListener('submit',async event=>{event.preventDefault();const form=new FormData(event.currentTarget);await runWrite(event.currentTarget,async()=>{await writeApi('/api/v1/keywords','POST',{word:form.get('word'),match_type:form.get('match_type'),category:form.get('category')||null,priority:Number(form.get('priority')),enabled:true});await keywords()})});
  document.querySelector('#keyword-test').addEventListener('click',event=>runWrite(event.currentTarget,async()=>{const text=window.prompt('输入要试跑的文本');if(text===null)return;const result=await writeApi('/api/v1/keywords/test','POST',{text});window.alert(result.matches.map(item=>item.word).join(', ')||'没有命中')}));
  document.querySelector('#keyword-export').addEventListener('click',()=>{window.location.href='/api/v1/keywords/export?format=json'});
  document.querySelector('#keyword-import').addEventListener('change',async event=>{const file=event.target.files?.[0];if(!file||event.target.dataset.pending)return;await runWrite(event.target,async()=>{const text=await file.text();const contentType=file.name.toLowerCase().endsWith('.csv')?'text/csv':'application/json';await writeRawApi('/api/v1/keywords/import',contentType,text);await keywords()});event.target.value=''})
  document.querySelectorAll('[data-keyword-toggle]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{await writeApi('/api/v1/keywords/'+button.dataset.keywordToggle,'PATCH',{enabled:button.dataset.enabled!=='true'});await keywords()})));
  document.querySelectorAll('[data-keyword-delete]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{if(!window.confirm('确定删除这个关键词吗？'))return;await writeApi('/api/v1/keywords/'+button.dataset.keywordDelete,'DELETE');await keywords()})));
  document.querySelectorAll('[data-keyword-edit]').forEach(form=>form.addEventListener('submit',async event=>{event.preventDefault();const data=new FormData(form);await runWrite(form,async()=>{await writeApi('/api/v1/keywords/'+form.dataset.keywordEdit,'PATCH',{word:data.get('word'),match_type:data.get('match_type'),category:data.get('category')||null,priority:Number(data.get('priority'))});await keywords()})}));
}

async function targets(){
  title.textContent='目标频道';
  const [body,identitiesBody]=await Promise.all([apiAllPages('/api/v1/targets'),apiAllPages('/api/v1/identities?kind=bot')]);
  const identityOptions=identitiesBody.items.map(item=>'<option value="'+item.id+'">'+escapeHtml(item.display_name||item.tg_user_id||item.id)+' · '+escapeHtml(item.kind)+'</option>').join('');
  const rows=body.items.map(item=>'<tr><td>'+escapeHtml(item.tg_chat_id)+'</td><td>'+escapeHtml(item.title||'—')+'</td><td>'+escapeHtml(item.sender_identity_id)+'</td><td>'+targetStatus(item)+'</td><td><div class="actions"><button data-target-toggle="'+item.id+'" data-enabled="'+item.enabled+'" type="button">'+(item.enabled?'停用':'启用')+'</button><button class="danger-button" data-target-delete="'+item.id+'" type="button">删除</button></div><details><summary>编辑</summary><form class="inline-form" data-target-edit="'+item.id+'"><input name="chat" value="'+escapeHtml(item.tg_chat_id)+'" required><input name="title" placeholder="名称" value="'+escapeHtml(item.title||'')+'"><select name="sender_identity_id" required>'+identitiesBody.items.map(identity=>'<option value="'+identity.id+'" '+(String(identity.id)===String(item.sender_identity_id)?'selected':'')+'>'+escapeHtml(identity.display_name||identity.tg_user_id||identity.id)+'</option>').join('')+'</select><button type="submit">保存</button></form></details></td></tr>');
  content.innerHTML='<div class="notice">目标频道当前要求填写已解析的 Telegram chat_id，例如 -1001234567890。创建后可启停和修改发送身份。</div><form id="target-form" class="toolbar"><input name="chat" placeholder="Telegram chat_id，例如 -100..." required><input name="title" placeholder="名称"><select name="sender_identity_id" required><option value="">选择发送身份</option>'+identityOptions+'</select><button>新增目标</button></form>'+(rows.length?renderTable(['频道','名称','发送身份','状态','操作'],rows):'<div class="empty">暂无目标频道。</div>');
  document.querySelector('#target-form').addEventListener('submit',async event=>{event.preventDefault();const form=new FormData(event.currentTarget);await runWrite(event.currentTarget,async()=>{await writeApi('/api/v1/targets','POST',{chat:form.get('chat'),title:form.get('title')||null,sender_identity_id:Number(form.get('sender_identity_id'))});await targets()})});
  document.querySelectorAll('[data-target-toggle]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{await writeApi('/api/v1/targets/'+button.dataset.targetToggle,'PATCH',{enabled:button.dataset.enabled!=='true'});await targets()})));
  document.querySelectorAll('[data-target-delete]').forEach(button=>button.addEventListener('click',()=>runWrite(button,async()=>{if(!window.confirm('确定停用这个目标频道吗？'))return;await writeApi('/api/v1/targets/'+button.dataset.targetDelete,'DELETE');await targets()})));
  document.querySelectorAll('[data-target-edit]').forEach(form=>form.addEventListener('submit',async event=>{event.preventDefault();const data=new FormData(form);await runWrite(form,async()=>{await writeApi('/api/v1/targets/'+form.dataset.targetEdit,'PATCH',{chat:data.get('chat'),title:data.get('title')||null,sender_identity_id:Number(data.get('sender_identity_id'))});await targets()})}));
}

async function rules(page=1,append=false,filters=ruleFilterState){
  if(ruleListLoading||document.querySelector('#rules-bulk-form')?.dataset.pending)return;
  ruleListLoading=true;
  const controls=[...(document.querySelector('#rules-filter-form')?.elements||[]),...(document.querySelector('#rules-bulk-form')?.elements||[]),document.querySelector('#rules-select-all'),...document.querySelectorAll('[data-rule-select]'),document.querySelector('#rules-load-more')].filter(Boolean);
  const previous=controls.map(control=>[control,control.disabled]);
  controls.forEach(control=>{control.disabled=true});
  try{
  title.textContent='规则列表';
  const params=new URLSearchParams({page:String(page)});
  for(const field of ['enabled','target_ref','source_ref'])if(filters[field])params.set(field,filters[field]);
  const body=await api('/api/v1/rules?'+params.toString());
  const rows=body.items.map(item=>'<tr>'+(canWrite?'<td><input type="checkbox" data-rule-select="'+escapeHtml(item.id)+'" data-enabled="'+String(item.enabled===true)+'" aria-label="选择规则 '+escapeHtml(item.id)+'"></td>':'')+'<td>'+escapeHtml(item.keyword)+'</td><td>'+escapeHtml(item.target_tg_chat_id)+'</td><td>'+escapeHtml(item.action)+'</td><td>'+escapeHtml(item.priority)+'</td><td><span data-rule-status>'+(item.enabled?'启用':'停用')+'</span>'+(canWrite?' · <a href="/console/rules/'+encodeURIComponent(item.id)+'">编辑</a>':'')+'</td></tr>');
  if(!append){
    ruleFilterState={...filters};
    content.innerHTML='<form id="rules-filter-form" class="toolbar"><select name="enabled" aria-label="规则状态"><option value="">全部状态</option><option value="true" '+(filters.enabled==='true'?'selected':'')+'>启用</option><option value="false" '+(filters.enabled==='false'?'selected':'')+'>停用</option></select><input name="target_ref" inputmode="numeric" placeholder="目标引用 ID" value="'+escapeHtml(filters.target_ref)+'"><input name="source_ref" inputmode="numeric" placeholder="来源引用 ID" value="'+escapeHtml(filters.source_ref)+'"><button type="submit" class="secondary">筛选</button><button id="rules-filter-clear" type="button" class="secondary">清除筛选</button></form><p class="muted">筛选成功后重置列表和选择；加载更多沿用筛选条件。引用 ID 可在规则编辑页查看。</p>'+(canWrite?'<div class="toolbar"><a class="button" href="/console/rules/new">新建规则</a><label><input id="rules-select-all" type="checkbox">选择已加载规则</label><span id="rules-selection-count" class="muted"></span><form id="rules-bulk-form" class="toolbar"><button type="submit" value="true">批量启用</button><button type="submit" value="false" class="secondary">批量停用</button></form></div><p id="rules-bulk-message" role="status" aria-live="polite"></p>':'')+'<div id="rules-list">'+(rows.length?renderTable([...(canWrite?['选择']:[]),'关键词','目标','动作','优先级','状态'],rows):'<div class="empty">暂无规则。</div>')+'</div><div id="rules-pagination" class="toolbar"></div>';
    bindRuleFilters();
  }else{
    document.querySelector('#rules-list tbody')?.insertAdjacentHTML('beforeend',rows.join(''));
  }
  const pagination=document.querySelector('#rules-pagination');
  if(!pagination)return;
  pagination.innerHTML=(body.has_more?'<button id="rules-load-more" class="secondary" type="button">加载更多</button>':'')+(body.page>1?'<span class="muted">已加载第 '+body.page+' 页</span>':'');
  const loadMore=document.querySelector('#rules-load-more');
  loadMore?.addEventListener('click',async()=>{
    if(ruleListLoading||loadMore.disabled||document.querySelector('#rules-bulk-form')?.dataset.pending)return;
    try{await rules(body.page+1,true)}catch(error){showError(error)}
  });
  bindRuleBulk();
  }finally{previous.forEach(([control,disabled])=>{control.disabled=disabled});ruleListLoading=false}
}
function bindRuleFilters(){
  const form=document.querySelector('#rules-filter-form');
  form.addEventListener('submit',async event=>{
    event.preventDefault();
    if(ruleListLoading||document.querySelector('#rules-bulk-form')?.dataset.pending)return;
    const data=new FormData(form);
    const filters={enabled:String(data.get('enabled')||''),target_ref:String(data.get('target_ref')||'').trim(),source_ref:String(data.get('source_ref')||'').trim()};
    try{await rules(1,false,filters)}catch(error){showError(error)}
  });
  document.querySelector('#rules-filter-clear').addEventListener('click',async()=>{
    if(ruleListLoading||document.querySelector('#rules-bulk-form')?.dataset.pending)return;
    try{await rules(1,false,{enabled:'',target_ref:'',source_ref:''})}catch(error){showError(error)}
  });
}
function bindRuleBulk(){
  const form=document.querySelector('#rules-bulk-form');
  if(!form)return;
  const all=document.querySelector('#rules-select-all');
  const count=document.querySelector('#rules-selection-count');
  const message=document.querySelector('#rules-bulk-message');
  function updateSelection(){
    const inputs=[...document.querySelectorAll('[data-rule-select]')];
    const selected=inputs.filter(input=>input.checked).length;
    all.checked=inputs.length>0&&selected===inputs.length;
    all.indeterminate=selected>0&&selected<inputs.length;
    count.textContent='已选 '+selected+' / '+inputs.length+' 条（仅已加载）';
  }
  document.querySelectorAll('[data-rule-select]').forEach(input=>{if(input.dataset.bound)return;input.dataset.bound='true';input.addEventListener('change',updateSelection)});
  updateSelection();
  if(form.dataset.bound)return;
  form.dataset.bound='true';
  all.addEventListener('change',()=>{document.querySelectorAll('[data-rule-select]').forEach(input=>{input.checked=all.checked});updateSelection()});
  form.addEventListener('submit',async event=>{
    event.preventDefault();
    if(form.dataset.pending)return;
    const enabled=event.submitter?.value;
    if(enabled!=='true'&&enabled!=='false')return;
    const loadMore=document.querySelector('#rules-load-more');
    if(ruleListLoading||loadMore?.disabled){message.textContent='列表正在加载，请稍后再操作。';return}
    const selected=[...document.querySelectorAll('[data-rule-select]')].filter(input=>input.checked);
    if(!selected.length){message.textContent='请先选择规则。';return}
    if(!window.confirm('确定'+(enabled==='true'?'启用':'停用')+'选中的 '+selected.length+' 条规则吗？逐条执行，失败不会回滚已完成项。'))return;
    await runWrite(form,async()=>{
      const controls=[all,...document.querySelectorAll('[data-rule-select]'),loadMore,...(document.querySelector('#rules-filter-form')?.elements||[])].filter(Boolean);
      const previous=controls.map(control=>[control,control.disabled]);
      controls.forEach(control=>{control.disabled=true});
      let completed=0;
      try{
        for(const input of selected){
          message.textContent='正在处理 '+(completed+1)+' / '+selected.length+' 条规则…';
          await writeApi('/api/v1/rules/'+encodeURIComponent(input.dataset.ruleSelect),'PATCH',{enabled:enabled==='true'});
          input.dataset.enabled=enabled;input.checked=false;
          input.closest('tr').querySelector('[data-rule-status]').textContent=enabled==='true'?'启用':'停用';
          completed+=1;
        }
        message.textContent='已完成 '+completed+' 条规则的'+(enabled==='true'?'启用':'停用')+'。';
      }catch(error){message.textContent='已确认完成 '+completed+' / '+selected.length+' 条；其余选择保留，请刷新页面核对后再重试。'+(error.message||'请求失败')}
      finally{previous.forEach(([control,disabled])=>{control.disabled=disabled});updateSelection()}
    });
  });
}

function conditionKindOptions(selected){
  return ['exclude_word','file_name','size_min','size_max','sender','ext_allow','ext_deny'].map(kind=>'<option value="'+kind+'" '+(kind===selected?'selected':'')+'>'+kind+'</option>').join('');
}
function conditionRow(condition,index){
  return '<div class="condition-row" data-condition-row="'+index+'"><select name="condition_kind">'+conditionKindOptions(condition.kind||'exclude_word')+'</select><input name="condition_value" value="'+escapeHtml(condition.value||'')+'" placeholder="条件值" required><button class="danger-button" data-remove-condition="'+index+'" type="button">删除</button></div>';
}
function replacementRow(replacement,index){
  return '<div class="replacement-row" data-replacement-row="'+index+'"><input name="replacement_pattern" value="'+escapeHtml(replacement.pattern||'')+'" placeholder="正则模式" required><input name="replacement_value" value="'+escapeHtml(replacement.replacement||'')+'" placeholder="替换文本"><label><input name="replacement_enabled" type="checkbox" '+(replacement.enabled!==false?'checked':'')+'>启用</label><button class="danger-button" data-remove-replacement="'+index+'" type="button">删除</button></div>';
}
function ruleVersionSnapshot(snapshot){
  if(!snapshot||!snapshot.rule||typeof snapshot.rule!=='object'||!Array.isArray(snapshot.conditions)||!Array.isArray(snapshot.replacements))throw new Error('历史快照格式无效，请刷新后重试。');
  const fields=['id','keyword_id','source_ref','media_type','target_ref','action','priority','stop_processing','template','version','enabled'];
  const rule=Object.fromEntries(fields.filter(field=>Object.hasOwn(snapshot.rule,field)).map(field=>[field,snapshot.rule[field]]));
  return {rule,conditions:snapshot.conditions.map(item=>({kind:item.kind,value:item.value})),replacements:snapshot.replacements.map(item=>({seq:item.seq,pattern:item.pattern,replacement:item.replacement,enabled:item.enabled}))};
}
function bindRuleHistory(id,currentVersion){
  if(!id){content.insertAdjacentHTML('beforeend','<section class="card"><h2>版本历史</h2><p class="muted">保存规则后可查看版本历史。</p></section>');return}
  content.insertAdjacentHTML('beforeend','<section class="card"><div class="top"><h2>版本历史</h2><button id="rule-history-refresh" class="secondary" type="button">加载 / 刷新版本历史</button></div><p class="muted">仅查看已保存的业务快照，不覆盖当前编辑内容，也不提供回滚操作。</p><p id="rule-history-message" role="status" aria-live="polite"></p><div id="rule-history-list"></div><pre id="rule-history-snapshot" hidden></pre></section>');
  const refresh=document.querySelector('#rule-history-refresh');
  const list=document.querySelector('#rule-history-list');
  const message=document.querySelector('#rule-history-message');
  const snapshot=document.querySelector('#rule-history-snapshot');
  let busy=false;
  refresh.addEventListener('click',async()=>{
    if(busy)return;
    busy=true;refresh.disabled=true;message.textContent='正在加载版本历史…';
    try{
      const body=await api('/api/v1/rules/'+encodeURIComponent(id)+'/versions');
      if(!Array.isArray(body.items)||body.items.some(item=>!Number.isInteger(item.version)||item.version<=0))throw new Error('版本列表格式无效，请重试。');
      const rows=body.items.map(item=>'<tr><td>'+escapeHtml(item.version)+(item.version===currentVersion?'（当前编辑版本）':'')+'</td><td>'+escapeHtml(item.created_at)+'</td><td><button class="secondary" type="button" data-rule-version="'+escapeHtml(item.version)+'">查看快照</button></td></tr>');
      list.innerHTML=rows.length?renderTable(['版本','创建时间','操作'],rows):'<div class="empty">暂无版本历史。</div>';
      snapshot.textContent='';snapshot.hidden=true;message.textContent='';
      document.querySelectorAll('[data-rule-version]').forEach(button=>button.addEventListener('click',async()=>{
        if(busy)return;
        busy=true;refresh.disabled=true;button.disabled=true;message.textContent='正在加载版本快照…';
        try{
          const detail=await api('/api/v1/rules/'+encodeURIComponent(id)+'/versions/'+encodeURIComponent(button.dataset.ruleVersion));
          if(String(detail.rule_id)!==String(id)||detail.version!==Number(button.dataset.ruleVersion))throw new Error('版本快照不匹配，请刷新后重试。');
          snapshot.textContent=JSON.stringify(ruleVersionSnapshot(detail.snapshot),null,2);snapshot.hidden=false;message.textContent='正在查看版本 '+detail.version;
        }catch(error){message.textContent=error.message||'加载版本快照失败'}finally{busy=false;refresh.disabled=false;button.disabled=false}
      }));
    }catch(error){message.textContent=error.message||'加载版本历史失败'}finally{busy=false;refresh.disabled=false}
  });
}
async function ruleEditor(){
  const id=path.endsWith('/new')?null:path.split('/').pop();
  title.textContent=id?'编辑规则':'新建规则';
  if(!canWrite){content.innerHTML='<div class="empty">当前角色没有规则编辑权限。</div>';return}
  const [keywordsBody,targetsBody]=await Promise.all([apiAllPages('/api/v1/keywords'),apiAllPages('/api/v1/targets')]);
  let current={keyword_id:'',target_ref:'',action:'FORWARD',priority:0,stop_processing:false,template:'',enabled:true,source_ref:null,media_type:null,conditions:[],replacements:[]};
  if(id){const detail=await api('/api/v1/rules/'+encodeURIComponent(id));current={...current,...detail.rule,conditions:detail.conditions||[],replacements:detail.replacements||[]}}
  const conditionRows=current.conditions.map(conditionRow).join('');
  const replacementRows=current.replacements.map(replacementRow).join('');
  content.innerHTML='<form id="rule-form" class="grid"><label>关键词<select name="keyword_id" required>'+keywordsBody.items.map(item=>'<option value="'+item.id+'" '+(String(item.id)===String(current.keyword_id)?'selected':'')+'>'+escapeHtml(item.word)+'</option>').join('')+'</select></label><label>目标频道<select name="target_ref" required>'+targetsBody.items.map(item=>'<option value="'+item.id+'" '+(String(item.id)===String(current.target_ref)?'selected':'')+'>'+escapeHtml(item.title||item.tg_chat_id)+'</option>').join('')+'</select></label><label>动作<select name="action"><option value="FORWARD">FORWARD</option><option value="COPY">COPY</option><option value="RESEND">RESEND</option></select></label><label>优先级<input name="priority" type="number" value="'+current.priority+'"></label><label>来源范围<input name="source_ref" type="number" placeholder="留空表示全部来源" value="'+(current.source_ref??'')+'"></label><label>媒体类型<select name="media_type"><option value="">不限</option><option value="ANY">ANY</option><option value="TEXT">TEXT</option><option value="PHOTO">PHOTO</option><option value="VIDEO">VIDEO</option><option value="AUDIO">AUDIO</option><option value="VOICE">VOICE</option><option value="VIDEO_NOTE">VIDEO_NOTE</option><option value="DOCUMENT">DOCUMENT</option><option value="STICKER">STICKER</option><option value="ANIMATION">ANIMATION</option><option value="OTHER">OTHER</option></select></label><label class="full">模板<textarea name="template" rows="4">'+escapeHtml(current.template||'')+'</textarea></label><label><input name="stop_processing" type="checkbox" '+(current.stop_processing?'checked':'')+'>停止继续处理</label><label><input name="enabled" type="checkbox" '+(current.enabled?'checked':'')+'>启用</label><div class="full card"><h3>条件</h3><p class="muted">同类条件按规格组合；size_min 与 size_max 各最多一条。</p><div id="condition-list">'+conditionRows+'</div><button id="add-condition" class="secondary" type="button">添加条件</button></div><div class="full card"><h3>替换</h3><p class="muted">按顺序执行，保存时整体提交。</p><div id="replacement-list">'+replacementRows+'</div><button id="add-replacement" class="secondary" type="button">添加替换</button></div><div class="full"><button>保存规则</button> '+(id?'<button id="rule-delete" class="danger-button" type="button">删除规则</button>':'')+' <a href="/console/rules">返回列表</a></div></form>'+(id?'<p id="rule-delete-message" role="status" aria-live="polite"></p>':'')+'';
  document.querySelector('[name=action]').value=current.action;
  document.querySelector('[name=media_type]').value=current.media_type||'';
  const conditionList=document.querySelector('#condition-list');
  const replacementList=document.querySelector('#replacement-list');
  document.querySelector('#add-condition').addEventListener('click',()=>{const index=conditionList.children.length;conditionList.insertAdjacentHTML('beforeend',conditionRow({kind:'exclude_word',value:''},index));bindDynamicRows()});
  document.querySelector('#add-replacement').addEventListener('click',()=>{const index=replacementList.children.length;replacementList.insertAdjacentHTML('beforeend',replacementRow({pattern:'',replacement:'',enabled:true},index));bindDynamicRows()});
  function bindDynamicRows(){
    document.querySelectorAll('[data-remove-condition]').forEach(button=>{if(button.dataset.bound)return;button.dataset.bound='true';button.addEventListener('click',()=>button.closest('.condition-row')?.remove())});
    document.querySelectorAll('[data-remove-replacement]').forEach(button=>{if(button.dataset.bound)return;button.dataset.bound='true';button.addEventListener('click',()=>button.closest('.replacement-row')?.remove())});
  }
  bindDynamicRows();
  const ruleForm=document.querySelector('#rule-form');
  document.querySelector('#rule-delete')?.addEventListener('click',()=>runWrite(ruleForm,async()=>{
    if(!window.confirm('确定删除规则 '+id+' 吗？将删除条件、替换和版本历史，不能撤销；已入队任务不会因此取消。'))return;
    const message=document.querySelector('#rule-delete-message');
    message.textContent='正在删除规则…';
    try{await writeApi('/api/v1/rules/'+encodeURIComponent(id),'DELETE');window.location.href='/console/rules'}
    catch(error){message.textContent='删除未确认，请先刷新列表核对，再决定是否重试。'+(error.message||'请求失败')}
  }));
  document.querySelector('#rule-form').addEventListener('submit',async event=>{
    event.preventDefault();
    const form=new FormData(event.currentTarget);
    const conditions=[...document.querySelectorAll('.condition-row')].map(row=>({kind:row.querySelector('[name=condition_kind]').value,value:row.querySelector('[name=condition_value]').value.trim()}));
    const replacements=[...document.querySelectorAll('.replacement-row')].map((row,index)=>({seq:index,pattern:row.querySelector('[name=replacement_pattern]').value,replacement:row.querySelector('[name=replacement_value]').value,enabled:row.querySelector('[name=replacement_enabled]').checked}));
    const sourceRef=String(form.get('source_ref')||'').trim();
    const body={keyword_id:Number(form.get('keyword_id')),source_ref:sourceRef?Number(sourceRef):null,media_type:form.get('media_type')||null,target_ref:Number(form.get('target_ref')),action:form.get('action'),priority:Number(form.get('priority')),stop_processing:form.get('stop_processing')==='on',template:form.get('template')||null,enabled:form.get('enabled')==='on',conditions,replacements};
    const url=id?'/api/v1/rules/'+encodeURIComponent(id):'/api/v1/rules';
    await runWrite(event.currentTarget,async()=>{await writeApi(url,id?'PATCH':'POST',body);window.location.href='/console/rules'});
  });
  bindRuleHistory(id,current.version);
}

loadMe().then(async authenticated=>{if(!authenticated)return;bindSessionControls();try{if(path==='/console/identities')await identities();else if(path==='/console/keywords')await keywords();else if(path==='/console/targets')await targets();else if(path==='/console/rules/new'||path.startsWith('/console/rules/'))await ruleEditor();else await rules();enforceReadOnly()}catch(error){showError(error)}}).catch(showError);
</script>
</body>
</html>"###;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::{CONSOLE_PAGE, LOGIN_PAGE};

    #[test]
    fn session_controls_execute_cookie_and_csrf_flows() -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_sessions.js")
    }

    #[test]
    fn login_controls_execute_serial_polling_and_failure_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(LOGIN_PAGE, "console_login.js")
    }

    #[test]
    fn business_writes_prevent_duplicate_submission_and_restore_controls()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_writes.js")
    }

    #[test]
    fn identity_intake_clears_credentials_and_serializes_login_states()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_identity_intake.js")
    }

    #[test]
    fn rule_history_loads_readonly_snapshots_without_changing_form()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_rule_history.js")
    }

    #[test]
    fn rule_list_bulk_state_changes_keep_partial_results_and_serialize_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_rule_bulk.js")
    }

    #[test]
    fn rule_list_filters_preserve_pagination_and_serialize_with_bulk_writes()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_rule_filters.js")
    }

    #[test]
    fn rule_editor_deletion_confirms_and_serializes_with_saving()
    -> Result<(), Box<dyn std::error::Error>> {
        check_page_script(CONSOLE_PAGE, "console_rule_delete.js")
    }

    fn check_page_script(page: &str, test_file: &str) -> Result<(), Box<dyn std::error::Error>> {
        let script = page
            .split_once("<script>")
            .and_then(|(_, tail)| tail.split_once("</script>"))
            .map(|(script, _)| script)
            .ok_or("页面缺少脚本")?;
        let mut process = Command::new("node")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .join(test_file),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        process
            .stdin
            .take()
            .ok_or("无法写入页面脚本")?
            .write_all(script.as_bytes())?;
        let output = process.wait_with_output()?;
        assert!(
            output.status.success(),
            "前端交互失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
