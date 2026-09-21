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
| `TGM_DB_URL_OWNER` | 仅 `tgm migrate` | `tgm_owner`。**常驻进程不得持有** |
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

### 4.1 必填项与角色匹配

按 `--role` 取该角色的必填集（§二 的表），缺失项列表一次性打印。
**多配的项也要报** —— `control-plane` 上出现 `TGM_LOCAL_KEK`
说明部署模板把所有变量喂给了所有角色，那就让 §二 的权限收敛失了效。

### 4.2 数据库版本下限

```sql
SHOW server_version_num;   -- 需 ≥ 170011（或 180006/160015/150019/140024）
```

低于下限直接拒绝启动。理由见 `../spec/05-安全与租户隔离.md`：
下限是 CVE-2026-14666 的修复版本，不是性能考量。

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

- §四 的六项校验**全部未实现、未实测**。其中 §4.3 的两段 SQL 我没在
  真实库上跑过（`psql` 未安装，见 `00-工程约定.md` §零）
- §三 的 `Kek` trait 只有签名。`async_trait` 与 `zeroize` 的交互
  （`Secret<[u8;32]>` 从 async 函数返回时的中间拷贝是否都被清零）**没有验证**，
  这是一个需要看汇编或用 `Drop` 断言才能确认的问题
- §二 的清单按当前设计推导，**必然会漏** —— 真实的漏项会在第一次部署时暴露。
  漏项的补法是补进本表，不是在代码里就地读一个新的环境变量
- 配置的**热重载不做**。改配置 = 重启进程。首发形态下这是可接受的，
  但它意味着调整日志级别也要重启
- KEK 轮换只有 `kek_id` 这个数据模型上的准备，**没有轮换流程**
  （谁触发、怎么灰度、旧 KEK 何时可停用）
