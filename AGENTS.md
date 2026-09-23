# AGENTS.md —— 给自主执行 agent 的入口约定

本文件回答一个问题：一个 agent（Codex CLI / Claude Code，或人）在这个仓库里
**怎么开工、怎么验收、什么时候必须停下来找人**。

本仓库把「AI 能不能被信任」的问题用机器断言回答过了（见
`docs/design/decisions/ADR-0022-CI断言替代第二人复核.md`）：安全面不设第二人
复核，改由断言在 CI 强制。所以这里的纪律只有一条主线——**一切以卡口与出口判据
为准，不以「看起来对」为准**。

> 本文件是索引，不是第二份事实源。数字与结论一律引用 `docs/` 里的唯一出处
> （`docs/design/README.md` 硬规则 1）。在本文件里复制任何工期、表数、断言条数
> 都是错误。

---

## 一、先读什么（按序，按需深读）

| 序 | 文件 | 回答什么 |
|---:|---|---|
| 1 | `docs/design/README.md` | 文档五层结构与三条硬规则（先读这个再碰别的） |
| 2 | `docs/design/eng/00-工程约定.md` | workspace 布局、错误类型、`.sqlx/` 纪律、评审清单 |
| 3 | `docs/design/eng/04-CI门禁.md` | **卡口的唯一定义处**：每条卡口的命令与失败信号 |
| 4 | `docs/design/eng/03-测试策略.md` | 测试分层（L1–L3）与反向测试基线（唯一清单在该文 §三，别处不写条数） |
| 5 | `docs/design/plan/00-里程碑与工期.md` §四 | 当前工作包与出口判据（任务从哪来） |
| 6 | `docs/plan/ponytail-debt.md` | 刻意简化的债务台账（只增不清，§4.4） |
| 7 | `scripts/ai-driver/refs/README.md` | 上游纪律事实源与台账（§4.0 / §6.1 / §6.2 的全文与锚点） |

其他层按需：规格问题查 `docs/design/spec/`，为什么这么定查 `docs/design/decisions/`
（只增不改），怎么部署查 `docs/design/ops/`。`docs/pre-do/` 是动工前清单，
其作废条件尚未满足（见该目录 `README.md`），其中的实测记录仍然有效。

**任务队列**：`docs/plan/backlog/`（机器可读，格式见该目录 `README.md`，
驱动从第一个 `status: pending` 取件）。

---

## 二、本地环境

- 一条命令起环境：仓库根 `docker compose up -d --wait`。定义处是
  `docs/design/eng/02-本地环境与迁移.md` §一；失败处置同文 §3.2：
  **本地一律 `docker compose down -v` 从零重建，不写 down 迁移**。
- 服务端口：PG 直连 `55432`，PgBouncer `56432`（只在跑隔离测试时用，
  见 `eng/02` §一），MinIO `59000`，Redis `56379`。
- 跑迁移走**生产路径**：`cargo run --bin tgm -- migrate`，不是 `sqlx migrate run`
  （理由见 `.github/workflows/ci.yml` 对应步骤的注释）。
- 环境变量命名与清单：`docs/design/eng/01-配置与Secret.md` §一。测试与审计常用的两条：

```bash
export DATABASE_URL=postgres://postgres:pw@127.0.0.1:55432/tgm        # sqlx 侧（含 sqlx::test 建库）
export TGM_DB_URL_OWNER=postgres://postgres:pw@127.0.0.1:55432/tgm     # tgm migrate / audit-rls
```

---

## 三、验收 = 卡口全绿 + 出口判据

提交前必须跑 `scripts/ai-driver/ai-gates.sh`（eng/04 §四 卡口清单的本地等价形式，
逐条对照 `.github/workflows/ci.yml`）。最终裁决在 CI；本地全绿只是入场券。

四个已实测的假绿陷阱，违反任何一条的「绿」不作数：

1. `cargo sqlx prepare --check` 无库时报 error 但 **rc=0** —— 它只能连着库跑。
2. 同一命令对 `.sqlx/` 多余条目只 warning、rc=0 —— 卡口靠
   `grep "unused queries found"` 转失败（ci.yml 里同款处理）。
3. 删 `.sqlx/` 后 `cargo build --workspace` 可能被增量编译骗过（rc=0），
   必须 `cargo build -p tgm-db` 强制重编译才见真章；本地反证这条时注意。
4. 审计断言全绿不等于隔离正确：`USING (true)` 能骗过全部静态断言
   （`eng/04` §五）。所以 `scripts/audit-double-pass.sh`（逐条破坏、恰好那一条红）
   是验收的一部分，不是可选项。

任务所属工作包的**出口判据**在 `plan/00` §4.3，逐条核完才算完成。

---

## 四、实现纪律

### 4.0 复用阶梯（ponytail full 档）

写代码前，在第一个成立的阶梯上停下（上游：github.com/DietrichGebert/ponytail，
全文与锚点见 `scripts/ai-driver/refs/ponytail.md`，正文保持英文原文）：

```text
1. Does this need to be built at all? (YAGNI)
2. Does it already exist in this codebase? Reuse it, don't rewrite.
3. Does the standard library already do this? Use it.
4. Does a native platform feature cover it? Use it.
5. Does an already-installed dependency solve it? Use it.
6. Can this be one line? Make it one line.
7. Only then: write the minimum code that works.
```

阶梯在**理解问题之后**用，不是代替理解：先读改动触及的代码、把真实链路走一遍，
再决定停在哪一级。错的地方最小的改动不是懒，是第二个 bug。

- 没有明确要求不加抽象；能不加依赖就不加；没人要的样板代码不写。
- Deletion over addition. Boring over clever. Fewest files possible.
- Bug 修复 = 根因不是症状：grep 你改的函数的所有调用方，共享函数里修一次。
  只修任务点名的那条路径，会把兄弟调用方留成坏的。
- 两个标准库方案同样大小时选边界情况正确的那个：懒是少写代码，不是要更脆的算法。
- 非平凡逻辑留下一个可运行的检查（逻辑坏掉就会失败的最小检查；本仓形状是
  `eng/03` 的测试分层，不另造形式）。平凡一行不需要测试。

**不得懒的地方**：理解问题、信任边界上的输入校验、防止数据丢失的错误处理、
安全与租户隔离、迁移幂等、平台校准（真实平台不符合理想规格 —— `eng/04` 里
全是这类实测，如 runner 的 `pg_dump` 版本）。以及一切被明确要求的东西。
没有检查的懒代码 = 未完成。

lite 档（审视范围）：写提案（`docs/plan/proposals/`）或评估新任务范围时只用前两问：
这件事是否需要存在？是否已有别的东西覆盖它？

### 4.1 高风险域：不启用简化档

下列域不做「一行就行」的简化，验证与分层结构不许减（原始纪律的六域是
SQL、权限、调度、执行记录、模型执行、发布包；本仓无模型执行，以信封加密
与凭据处理替入，锚点如下）：

```text
SQL          migrations/、审计 SQL、repo 层 query! 宏
权限         RLS 策略、GRANT、角色、租户上下文的代码形态（eng/00 §四）
调度         account_leases 租约、后台任务调度
执行记录     outbox、delivery_logs、audit_logs
加密与凭据   信封加密、KEK trait、Secret<T> 脱敏
发布与部署   ops/ 工件、compose.yaml、ci.yml
```

阶梯与这六域冲突时，以本节为准。

> 替换决策的登记：上游六域里的「模型执行」在本仓以「加密与凭据」替入，
> 这是实现期的实质适配，登记在 `scripts/ai-driver/refs/README.md` 的台账里；
> `plan/00` §6.4 的演进候选项含 AI 语义过滤，届时回来重核本节。

### 4.2 不得以 YAGNI 为由删除

`docs/design/` 的五层文档层级与三条硬规则、`eng/04` 的卡口定义、§4.1 高风险域
的逻辑、`backlog` / `proposals` 的任务工件 —— 不得以 YAGNI 或 ponytail 为由
删除或掏空。

### 4.3 本仓硬纪律（最容易违反的几条）

- **新建表 → 自动落入审计。** 判据是「表里有没有 `tenant_id` 列」而非人工清单，
  带 `tenant_id` 的新表必须开齐 RLS 三件套（ENABLE + FORCE + 策略），
  形状照 `docs/design/spec/05-安全与租户隔离.md` 的模板。
- **迁移只增不改，不写 down 迁移**（ADR-0021）。每个 DDL 文件自身幂等，
  判据是 `scripts/migrate-idempotent.sh`。
- 隔离测试**必须以受限角色连接**。测试框架注入的池是超级用户池，旁路 RLS，
  用它跑隔离测试得到的是假绿（`plan/00` WP-1 硬要求 2）。
- 错误：`thiserror` 给库层，`anyhow` 只给 `bins/`；`sqlx::Error` 不得透给客户端
  （`eng/00` §三）。
- 版本统一写在根 `Cargo.toml` 的 `[workspace.dependencies]`（eng/00 §6.3）。
- SQL 改动后重跑 `cargo sqlx prepare --workspace`；`crates/db/src/audit/queries.sql`
  与 `eng/04` 两处必须同步，不一致时**以实现为准**（见 `eng/04` 开头）。
- 断言引用用编号（`A1`~`A9`、`R1`~`R24`），不写条数（数量会变，编号稳定）。
- lint 是硬约束：`unsafe_code`/`unwrap_used`/`todo`/`dbg_macro` 在
  workspace lints 里是 deny，不要用 `#[allow]` 绕。

### 4.4 刻意简化：注释、复查、台账

- 带已知天花板的刻意简化（全局锁、O(n²) 扫描、朴素启发式）必须留
  `ponytail:` 注释，格式 `ponytail: <天花板>，<升级触发>`：

```rust
// ponytail: O(n²) 扫描，keywords 超过一万条时改哈希索引
```

- 开 PR 前（或代码审查前）对实现 diff 做一遍 `ponytail-review`：有没有漏掉的
  复用机会、没有要求的抽象、被裁掉又没补检查的逻辑。
- 任务归档后（PR 合并、backlog 状态 `done`），把留下的 `ponytail:` 注释登记进
  `docs/plan/ponytail-debt.md` 台账：位置、天花板、升级触发。台账只增不清。
- 本仓的 agent 都是无头执行、不加载上游技能包，本节是等价纪律；
  上游全文带 commit 锚点在 `scripts/ai-driver/refs/`，与上游对齐的更新流程见
  那里的 `README.md`。执行输出总结用中文。

---

## 五、任务循环与硬边界

驱动在 `scripts/ai-driver/`（说明见该目录 `README.md`）：
任务文件 → 干净 worktree → 起库 → agent 实现 → 卡口全绿 → 推分支开 PR。
人只保留合并决策；涉及下列事项时，agent **必须升级**：写一份
`docs/plan/proposals/<任务id>.md` 提案，提交后停止，不直接实现。

升级触发条件（命中其一即停）：

```text
1 新建表 / 改表结构 / 新增迁移文件
2 改动已应用的迁移文件（禁止，无例外）
3 安全面改动：RLS 策略、角色与 GRANT、审计断言、eng/04 卡口本身、ci.yml
4 需要写新 ADR，或触碰基线冻结文档的变更分级 A 类（plan/00 §九）
5 需要 Telegram 真机：QR 扫码、2FA、DC 迁移、真凭据
6 任务卡壳且修复需要改动上面 1~5 的任何一项
```

---

## 六、写作与提交（humanizer-zh + caveman-commit）

本仓库的**交付物有一半是散文**：spec/、eng/、decisions/、plan/、ADR、
commit body、PR 描述、提案。这些由 AI 生成时必须过同一道「中文润色」，
规则的事实源是 `scripts/ai-driver/refs/humanizer-zh.md`（上游 op7418/Humanizer-zh
的 SKILL.md 全文，带 commit 锚点，正文中文原样保留）。

### 6.1 散文润色（humanizer-zh，full 档）

写或改任何 `docs/` 下的中文、commit body、PR 描述、提案时，遵循该文件：
保留事实与确定程度、保留作者声音、处理空话与模板化表达。要点：

```text
1 不增不造：不补写原文没有的事实、数字、性能结论、归因；
  不把「可能」写成「确定」、把「计划」写成「已完成」。
2 文件保护：默认只编辑散文正文。代码块、命令、路径、SQL、
  显式 ID、表格数据保持原样 —— 与卡口相关的东西一字不动。
3 标题与锚点：保留文件的标题文字、层级与数量，避免破坏自动锚点。
  本仓库的「编号稳定」纪律（断言用编号不用条数）正是这一条的特例。
4 已经清楚的句子不改；不为了展示工作量强改。
```

本仓库的文体是**工程说明文**：准确、可核对、带实测证据。
humanizer-zh 的「技术文档」一节适用；它不要求口语化，也不删正式表达。

### 6.2 提交（caveman-commit 纪律）

上游全文与锚点：`scripts/ai-driver/refs/caveman-commit.md`
（JuliusBrussee/caveman `skills/caveman-commit/SKILL.md`）。原则是短、准、
可粘贴，代码、命令、路径与精确报错**不压缩**，只压散文。
本仓与上游的分叉逐条登记在事实源的「本地出处说明」节。

- 一任务一提交；分支名 `codex/<任务id>`。
- 生成前先 `git diff --staged` 查看暂存区；**没有 staged diff 就不生成**，
  明说「无法从暂存区生成提交信息」，不凭空编。
- 用 Conventional Commits：`<type>(<scope>): <subject>`。type 与 scope 保持英文；
  subject 与 body 用**中文**书写，类名、字段名、路径等技术术语保留原文。
  （现有历史里的 `<type>: … —— …` 变体是旧风格，新提交统一走本条；
  caveman 上游要求英文祈使句摘要，与本条冲突时**以本条为准**。）
- **subject 与 body 都过一遍 6.1**：删掉套话、拔高与铺垫，但保留
  判据出处、反证记录与所有技术事实。
- 涉及下列任一必须写 body，说明**原因与影响**：安全面（RLS/GRANT/审计）、
  SQL 与迁移、调度（租约/后台任务）、执行记录（outbox/日志/审计表）、
  加密与凭据、发布与部署工件、配置外置、revert。
- body 照仓库现有风格：写清判据怎么过的、反证做了什么；不写「AI 生成」类尾注。

### 6.3 PR 描述与提案

必须含：任务 id、验收判据出处（`plan/00` §几 / `eng/03` 哪条 R）、
卡口执行结果；升级任务附提案路径。正文过 6.1。

示例：

```text
test(db): R18 并发应答只推进一个 —— 照字面写会退化成顺序执行

判据出处：eng/03 §三 R18、§3.6 的并发形状教训。
两个连接同时打到 pending，恰好一个推进；以受限角色连接验证。
反证：去掉事务包裹后测试退化成顺序执行，本版本能逮住。
```

---

## 七、无头执行命令（驱动脚本使用）

```bash
# Codex CLI（自动读取 AGENTS.md）
codex exec --dangerously-bypass-approvals-and-sandbox "$(cat prompt)"

# Claude Code（经仓库根的 CLAUDE.md 符号链接读取 AGENTS.md）
claude -p --dangerously-skip-permissions "$(cat prompt)"
```

两个执行器都必须自己保证 `DATABASE_URL` / `TGM_DB_URL_OWNER` 与库就位；
驱动脚本已注入，会话内不要改它们的值。

**信任边界（必读）**：两个执行器都以全权限、无沙箱运行，而提示词的输入之一是
`docs/plan/backlog/` 里的任务文件 —— 普通仓库文件。所以：

```text
1 backlog 只能由持有合并权的人写入或批准写入；经外部 PR 投毒一条任务，
  就等于给全权限执行器喂了一条指令。这是本驱动的第一信任边界。
2 保护路径（scripts/、migrations/、.github/、crates/db/src/audit/ 等，
  清单在 scripts/ai-driver/ai-run.sh 的 PROTECTED）被任务改动时，
  驱动直接判升级、不跑卡口 —— 这是机器判据，不依赖提示词自觉。
3 本文件与 backlog 也在保护清单里：下一轮的提示词由 AGENTS.md 与任务文件
  拼成，执行器改一行本文件 §五 的升级条件、或把自己任务的 escalation
  改成 no，就是自我减刑。第 1 条管人写入，这一条管执行器回写。
4 执行器产出的分支永远经 PR 合并，驱动不合并；卡口绿只是入场券，
  CI 那一次真跑才是裁决。
```

---

## 八、完成汇报格式

任务结束时输出一段固定格式的汇报（驱动脚本靠它判断结局）：

```text
[report]
task: <任务id>
path: ok | escalated | blocked
gates: pass | fail | not-run
escalation: <提案文件路径或 ->
```

`blocked` 时必须说明卡在哪条判据、试过什么。不要为凑 `ok` 放宽判据 ——
判据不满足就是任务未完成，这是本仓库反复记录的教训：
**判据满足了，不等于它想保证的性质成立**；反过来，判据没满足而声称完成，
比明说未完成更贵。
