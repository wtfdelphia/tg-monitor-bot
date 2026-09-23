# 配置与 Secret

本文是**配置项与密钥处理的唯一定义处**：配置从哪来、启动时校验什么、
KEK 的两种实现怎么共存。

信封加密的原理与「哪些组件有解密权」见 `../spec/05-安全与租户隔离.md` §七，
不在这里重复；本文只写落地形态。依赖版本见 `00-工程约定.md` §二。

---

## 一、唯一来源：环境变量

```text
生产   环境变量，由部署方的 secret 注入机制写入（见 ../ops/00-部署与运行时.md）
本地   .env（已在 .gitignore 里，不得提交）
```

**不引配置文件库，不做「文件 + 环境变量 + 命令行」三层覆盖。**
理由：三层覆盖的真实成本不是实现，是**排查时说不清某个值究竟从哪来** ——
而这个系统里说不清的那个值可能是 KEK 标识或数据库角色。
一层来源意味着 `env | sort` 就是完整事实。

两条硬约束：

```text
1 Secret 不走命令行参数 —— 同机任何进程 ps 可见
2 Secret 不写进镜像、不写进 compose.yaml 本体（用 env_file 指向 .env）
```

`--role` 是唯一的命令行参数（`00-工程约定.md` §一），它不是 secret。

---

## 二、配置项清单

按 `--role` 分组。**某个角色不需要的项，就不该配给它** ——
这不是整洁，是最小权限的可执行形式：
`control-plane` 拿不到 KEK 相关配置，它就**不可能**解密租户凭据。

| 变量 | 需要它的角色 | 说明 |
|---|---|---|
| `TGM_PROFILE` | 全部 | `production` / `development`，决定 §四 的严格程度 |
| `RUST_LOG` | 全部 | 默认 `info,sqlx=warn`，见 §4.5 |
| `TGM_DB_URL_APP` | 全部 | `app_user` 的连接串，经 PgBouncer |
| `TGM_DB_URL_AUTH` | control-plane / bot-gateway | `auth_lookup` 的独立池（`../spec/05-安全与租户隔离.md` §3.7） |
| `TGM_DB_URL_OPS` | control-plane | `platform_ops`，只供 `admin_tx`（`00-工程约定.md` §4.2） |
| `TGM_DB_URL_OWNER` | 仅 `tgm migrate` | 持有 `tgm_owner` 的角色。`tgm_owner` 自身 `NOLOGIN`，本地为 `postgres`。**常驻进程不得持有** |
| `TGM_REDIS_URL` | 全部 | 仅唤醒信号，无正确性依赖 |
| `TGM_S3_ENDPOINT` / `TGM_S3_BUCKET` | delivery / mtproto | MinIO 或 S3 |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | delivery / mtproto | 由 `aws-config` 读取，名字固定 |
| `TGM_KEK_BACKEND` | bot-gateway / mtproto / delivery | `kms` / `env`，见 §三 |
| `TGM_KMS_KEY_ARN` | 同上（`backend=kms`） | KEK 在 KMS 里的标识 |
| `TGM_LOCAL_KEK` | 同上（`backend=env`） | base64 的 32 字节。`production` 下拒绝启动 |
| `TGM_OIDC_BOT_ID` | control-plane | `id_token.aud` 的期望值 |
| `TGM_OIDC_ALG` | control-plane | **验签算法从这里读，不从 token header 读** |
| `TGM_OIDC_REDIRECT_URL` | control-plane | 必须与 BotFather 登记的白名单一致 |
| `TGM_CONTROL_BOT_TOKEN` | control-plane | 控制 Bot 的 token（平台自己的，非租户凭据） |
| `TGM_LISTEN_ADDR` | control-plane | 默认 `127.0.0.1:8080`，见下方 |

> **`TGM_LISTEN_ADDR` 默认绑回环，不绑 `0.0.0.0`。**
> 控制面自身**不做** TLS 终止与公网暴露，那是反向代理的职责
> （`../ops/00-部署与运行时.md`）。默认值选回环的理由是
> 配错时的失败方向：绑回环配错 = 连不上（吵闹），
> 绑 `0.0.0.0` 配错 = 无鉴权前置的服务直接上公网（安静）。

`TGM_DB_URL_OWNER` 那一行是硬纪律：owner 连接串只在迁移时存在。
若常驻进程能拿到它，`../spec/05-安全与租户隔离.md` §3.3 纪律 1
（应用角色非 owner）在运行时就失去意义 —— 不是策略被绕过，
而是有人可以用 owner 身份连进来。

> **这一格原先只写「`tgm_owner`」，读起来像个可以直接填进连接串的角色名。**
> 它不是：`scripts/init/01-roles.sql:3` 建的是 `CREATE ROLE tgm_owner NOLOGIN`
> —— 没有 LOGIN、没有密码，拿它做连接串必然
> `password authentication failed for user "tgm_owner"`。
> owner 身份靠同文件的 `GRANT tgm_owner TO postgres` 继承，
> 再由每个迁移文件自己 `SET ROLE tgm_owner`
> （`02-本地环境与迁移.md` §3.0）。本地实际可用的值是
> `postgres://postgres:pw@127.0.0.1:55432/tgm`。
>
> 这一格的意图（owner 权限只在迁移时存在）没变，变的是它不再像个角色名。
> 连带一处**没改的**：`scripts/audit-double-pass.sh` 把这个认证失败显示成
> `✗ 第一遍就是红的 —— 先修库`，把读者指向库的配置，而真因在连接串。
> 脚本没改（区分「红是因为断言还是因为连不上」要多一次探测，
> 收益不及复杂度）—— 撞上时先看 stderr 里有没有认证报错。

---

## 三、KEK 的两种实现

本地开发不该依赖云 KMS，所以从第一天起就要有两个实现共用一个 trait。
**这是当下就成立的需求，不是为将来预留**。

```rust
// crates/core/src/kek.rs
#[async_trait]
pub trait Kek: Send + Sync {
    /// 用 KEK 包裹 DEK，产出 credential_keys.dek_ciphertext
    async fn wrap(&self, dek: &Secret<[u8; 32]>) -> Result<Vec<u8>>;
    /// 解开 DEK。返回值用后必须 zeroize
    async fn unwrap(&self, ciphertext: &[u8]) -> Result<Secret<[u8; 32]>>;
    /// 写入 credential_keys.kek_id，用于将来分辨轮换代次
    fn kek_id(&self) -> &str;
}
```

```text
KmsKek   生产。调 KMS 的 Encrypt / Decrypt，KEK 明文永不出 KMS
         kek_id = KMS 的 key ARN
EnvKek   本地。从 TGM_LOCAL_KEK 取 32 字节，AES-256-GCM 包裹 DEK
         kek_id = "local:env:v1"
```

`kek_id` 是列而不是常量的理由：它让 KEK 轮换可以增量做 ——
新 DEK 用新 KEK 包，旧行按自己的 `kek_id` 找对应 KEK 解开，
不需要一次性重包全部租户。`EnvKek` 的 `kek_id` 里带 `v1` 是同一个道理。

> **`EnvKek` 在 `TGM_PROFILE=production` 下必须拒绝启动。**
> 它的安全属性与 `KmsKek` 差一个量级（KEK 明文在进程环境里），
> 拿到库 + env 就能全解 —— 这正是本方案不采用「Fernet + 环境变量密钥」的理由。
> 若允许它在生产跑，信封加密就退化成了那个被否掉的方案。
> 这条检查放在 §四 的启动期，不是文档里的提醒。

DEK 的使用边界：`unwrap` 出来的明文只在一次加解密调用的生命周期内存在，
用后 `zeroize`。**不缓存解开后的 DEK** —— 缓存会把「进程内存快照」
变成一次全租户凭据泄漏，而 KMS 调用的成本远低于这个风险。

---

## 四、启动期校验

**原则：能在启动时发现的配置错误，绝不留到运行时。**
运行时发现的配置错误在这个系统里的形态通常是「静默少查了数据」或
「凭据解不开」，两者都难归因。

一条元规则：**把全部缺失项收集齐再一次性报错**，不要撞到第一个就退出 ——
按角色配十几个变量，逐个试错要重启十几次。

这条元规则最容易被**实现顺序**破掉，而不是被哪一条检查破掉：
`TGM_PROFILE` 是下面几项的输入（§4.5 要它判 production），所以最自然的写法是
先解析它、失败就早退 —— 于是它缺失或拼错时只报它一条，同时缺的别的变量
下一轮才看见。实际落地的形状是 `config::check` 收**未解析的原始值**
（`Option<&str>`），解析在函数内、失败也只是一条 problem。
代价是 profile 定不下来时 §4.5 那条**判不了**，所以它那两条报错的文案里
带了「§4.5 的日志过滤器检查跳过」—— 一条没跑的检查不能长得像一条通过的检查。

### 4.1 必填项与角色匹配

按 `--role` 取该角色的必填集（§二 的表），缺失项列表一次性打印。
**多配的项也要报** —— `control-plane` 上出现 `TGM_LOCAL_KEK`
说明部署模板把所有变量喂给了所有角色，那就让 §二 的权限收敛失了效。

### 4.2 数据库版本下限

```sql
SELECT current_setting('server_version_num')::int;
-- 需 ≥ 180006 / 170011 / 160015 / 150019 / 140024，按大版本各自取下限
```

低于下限直接拒绝启动。理由见 `../spec/05-安全与租户隔离.md`：
下限是 CVE-2026-14666 的修复版本，不是性能考量。

落地时（`crates/db/src/preflight.rs`）三处偏离了上面这段草图：

```text
1  用 current_setting 而不是 SHOW。SHOW 是 utility 命令、返回列名依版本而变；
   前者是普通函数调用，可以直接 query_scalar。
   经 PgBouncer 的 transaction 模式实测可透传（返回 170011），
   所以这一条不需要旁路直连

2  下限不是一个数字，是 (大版本, 下限) 的表。
   写成「≥ 170011」会让 18.0（=180000）被判为合规 —— 它比 170011 大，
   却没打 18 系的补丁。实测断言里 180000 必须红在 floor=180006

3  不在表里的大版本必须拒绝，不能放过。
   「不认识就通过」会让这条检查在下一个大版本发布时静默失效
   （13 与 19 都报 UnknownMajor）
```

判定逻辑与取数拆成两个函数，理由是**否定用例跑不起来**：
手边只有一个 17.11 的库，`VersionTooOld` 这条路径在集成测试里永远走不到，
而走不到的分支与不存在的分支在测试结果上长得一样。
拆开之后那两条路径在单元测试里可达，并且每个列出的大版本都验了
「下限过、下限减一红」。

### 4.3 运行时角色自检

这是启动期最有价值的一条，因为它把一条只能靠人看的纪律变成了机器断言：

```sql
SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user;
-- 两者都必须为 false，否则拒绝启动
```

`../spec/05-安全与租户隔离.md` §3.3 纪律 1 的原话是「配错了策略形同虚设，
而且不会报错 —— 静默失效」。上面两行 SQL 就是给那句「不会报错」补的报错。

同时检查**当前角色不是表 owner**：

```sql
SELECT count(*) FROM pg_tables
 WHERE schemaname = 'public' AND tableowner = current_user;
-- 必须为 0
```

这一条的失败方向不是「泄漏」而是「静默 0 行」：owner 受 `FORCE` 约束，
但通常没有匹配它的策略（`03-测试策略.md` §五 实测）。
两个方向都不可接受，所以一样拒绝启动。

**上面第一段 SQL 有一个缺口：继承自组的 BYPASSRLS 它查不到。** 实测：

```text
建一个 NOLOGIN 组角色带 BYPASSRLS，登录角色 IN ROLE 它，两者都 GRANT app_user
→ 登录角色自己的 rolbypassrls 是 f，上面那句 SQL 判为通过
→ SET ROLE app_user + app.tenant_id 填一个不存在的租户  → keywords 0 行
→ SET ROLE 到那个组                                     → 同一张表 2 行
```

所以实际落地的判据加了一段（`crates/db/src/preflight.rs`，
文中省掉了代码里的 `::text` 别名与 `ORDER BY 1 LIMIT 1` ——
后者只为取一条做报错文案，不影响判据）：

```sql
SELECT r.rolname, r.rolsuper, r.rolbypassrls
  FROM pg_roles r
 WHERE pg_has_role(current_user, r.oid, 'USAGE')
   AND r.rolname <> current_user
   AND (r.rolsuper OR r.rolbypassrls);
-- 必须 0 行
```

集成测试 `membership_in_a_bypassrls_role_is_caught` 钉的是这一段，
但它断言的是**报错的形状**（`role=pf_member` / `grantor=pf_group` /
`attr=BYPASSRLS`），不是上面那条 0 行 → 2 行的旁路本身 ——
后者是手工复现的，没进自动化。

`rolname <> current_user` 那一句是必需的：`pg_has_role` 对自己恒为真，
不排掉它则第一段 SQL 的结论会被这一段重复一遍。而**不要**把它放宽成
「排掉所有超级用户」—— 那会让这条断言对超级用户运行时永不触发
（同一个坑在审计断言 A2a 上踩过，见 `04-CI门禁.md`）。

校验通过时把**实际生效的角色名打进启动日志**。那一行是事后唯一能回答
「那次运行到底用的哪个角色」的证据。

### 4.4 KEK 往返自测

启动时对一个常量做一次 `wrap` → `unwrap` → 比对。
理由：KMS 的权限问题（key policy 写错、IAM 角色没绑上）
在不调用时完全看不出来，而第一次真实调用发生在某个租户添加凭据的时候 ——
那时的报错会被当成业务 bug 查。

`backend=env` 时这一步顺带校验 `TGM_LOCAL_KEK` 确实是 32 字节。

### 4.5 日志过滤器

若 `RUST_LOG` 被显式指定且不含 `sqlx=warn`，在 `production` 下拒绝启动。
理由见 `00-工程约定.md` §五 第 3 条：sqlx 把绑定参数打进日志，
而 `identity_secrets` 的写入语句绑定的就是密文与 nonce。
这不是日志噪音问题，是密钥材料入日志。

### 4.6 不放在启动期的两件事

```text
RLS 配置是否完整   → tgm audit-rls（04-CI门禁.md）。它要读 pg_class/pg_policy，
                    属审计口径，放启动期会让每次重启都跑一遍全表扫描
会话变量残留       → 属隔离测试（03-测试策略.md）。它只在连接被复用之后才出现，
                    启动时连接池是全新的，此时检查必然通过 —— 假绿灯比不检查更坏
```

第二条值得记住：**一个必然通过的检查不是保障，是噪音。**

---

## 五、本文的局限

- §四 的六项校验**已全部实现并端到端实测**（`bins/tgm/src/main.rs` 的
  `preflight()` + `crates/db/src/preflight.rs`）。跑 `tgm serve --role ...`
  验过的场景：缺 `TGM_PROFILE`（与同时缺的另两项一次报三条）、
  `TGM_PROFILE=prod` 拼错（同样一次报三条，不是只报 profile 自己）、
  只给 `TGM_PROFILE`（一次性报两条缺失）、配齐通过、
  多配 `TGM_DB_URL_OWNER` 被拒、
  连接串指向超级用户被拒、`production` + `RUST_LOG=debug` 被拒而
  `development` 放过、KEK 32 字节通过 / 31 字节被拒 /
  `production` 下 `EnvKek` 被拒。另有两个分支只在集成测试里可达
  （`crates/db/tests/preflight.rs`）：**运行时角色是 public 表的 owner**
  与**从组继承来的 BYPASSRLS** —— 它们都要先造一个探针角色，
  拿现成的 `postgres` 测会在 `rolsuper` 那句先红，走不到这两个分支。
  **仍然欠着的是 `backend=kms`** —— 那条路径只打一行 `NOT IMPLEMENTED`
  （刻意可见：一个静默跳过的自测与通过的自测同形），`KmsKek` 待落地
- §三 的 `Kek` trait 的 `Secret` 清零**只做到 bound 层面**：
  `Zeroize` 的 bound 是 `E0367` 逼出来的（`00-工程约定.md` §五
  那段 `Secret<T>` 草图照抄编不过，`Drop` 里要清零就得给 `T` 加 bound）。
  真正没验证的是**入包之前的中间拷贝** ——
  `Secret<[u8;32]>` 从 async 函数返回时途经的临时值不在 `Drop` 的管辖范围内，
  这需要看汇编或用 `Drop` 断言才能确认，目前都没做
- §二 的清单按当前设计推导，**必然会漏** —— 真实的漏项会在第一次部署时暴露。
  漏项的补法是补进本表，不是在代码里就地读一个新的环境变量
- 配置的**热重载不做**。改配置 = 重启进程。首发形态下这是可接受的，
  但它意味着调整日志级别也要重启
- KEK 轮换只有 `kek_id` 这个数据模型上的准备，**没有轮换流程**
  （谁触发、怎么灰度、旧 KEK 何时可停用）
