# WP-7：控制台登录二维码交付

任务 id：`WP-7-console-login-qr-image`。

状态：维护人于 2026-10-23 批准单独实施，并批准将浏览器请求的 `cache: no-store` 纳入本片范围；本提案冻结交付形态与安全边界，实施和验收结果见 `docs/plan/evidence/WP-7-控制台登录二维码-2026-10-23.md`。

## 一、现状与判据

`POST /auth/qr/challenge` 已创建短时控制台登录挑战，并返回 nonce、确认码、过期时间和 Telegram 深链接；登录页目前只展示链接，没有可供手机相机扫描的图像。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-7；
`docs/design/spec/07-接口契约.md` §1.3、§3.1。
WP-7 整体仍未通过；目标频道连通性自检不属于本提案。

## 二、建议范围

在现有挑战响应中增加 `qr_svg` 字段，由服务端复用现有 `render_qr_svg`，对与 `deep_link` 完全相同的 Telegram URL 生成 SVG。响应继续使用现有 `no-store` 中间件；不另加图片路由，不写入数据库，也不改变挑战状态机、有效期或会话签发逻辑。

登录页将 SVG 放进独立 `<img>`，不使用 `innerHTML` 或把 SVG 当 HTML 插入。页面校验响应形状及 SVG 根元素、命名空间和允许的元素/属性后再展示；二维码加载失败时保留已校验的深链接与确认码作为替代入口。过期、拒绝、确认、轮询失败、重试和离页时清除图像引用、深链接与确认码；挑战轮询仍串行执行，只有 `confirmed` 才跳转。

新增字段属于公开扫码挑战响应的一部分，应同步更新 `QrChallengeResponse` 的 OpenAPI schema、公开 OpenAPI 工件及 `docs/design/spec/07-接口契约.md`。不得把 MTProto 账号接入二维码混入此链路，也不得将挑战 nonce 写入额外日志、持久存储或截图证据。

## 三、边界与安全要求

- 不新增表、迁移、角色、GRANT、路由或依赖；不改 `console_login_challenges` 状态机和现有限流。
- 二维码只编码现有 `https://t.me/<bot>?start=login_<nonce>` 深链接，不编码 session 或认证 Cookie。
- 创建挑战响应已有 `Cache-Control: no-store`；浏览器请求同源发送，显式使用 `cache: no-store`，页面不把 SVG 写入浏览器持久存储。
- SVG 只能用图像元素渲染；校验失败则丢弃图像，不放宽为 HTML 插入，也不把服务端原始错误回显。
- 若实现发现挑战材料进入访问日志、需要改变认证/授权、数据库或现有状态机，停止并另行提案，不在本片扩大范围。
- 本片仅验证登录挑战与图像交付，不执行 Telegram 真机扫码或确认，不宣布 WP-7 通过。

## 四、验收

- `crates/app/tests/console_login.js` 覆盖挑战响应校验、图像 URL、深链接与确认码一致性、创建失败重试、终态/过期清理、离页清理、串行轮询与确认跳转；加入破坏响应校验的反证。
- `crates/app/tests/control.rs` 验证二维码由现有深链接生成，挑战接口仍返回 `no-store`，且登录签发行为不变。
- 显式运行浏览器验收：确认图像成功解码、窄屏不横向溢出、键盘操作正常；使用本地测试环境，不请求 Telegram 外部服务，不截图保留二维码。
- 更新 OpenAPI 工件后运行 `tgm openapi --check`，再运行 `scripts/ai-driver/ai-gates.sh`；最终由目标分支 PR CI 裁决。

## 五、预期改动

预期涉及 `crates/app/src/console.rs`、`crates/app/src/control.rs`、对应页面与浏览器测试、`openapi.json` 及 `docs/design/spec/07-接口契约.md`。不触碰迁移、数据库权限、`ci.yml` 或目标频道自检。

[report]
task: WP-7-console-login-qr-image
path: ok
gates: pass
escalation: ->
