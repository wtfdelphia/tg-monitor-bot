# ai-driver —— AI 自主驱动循环

把「人写代码、机器验收」改成「机器写代码、机器验收、人只做合并决策与升级裁决」。
能成立的根据不在这里：它依赖 `docs/design/eng/04-CI门禁.md` 的卡口体系与
ADR-0022（断言替代第二人复核）。本目录只是那条结论的执行器。

## 文件

```text
ai-run.sh     单任务驱动器：选取 → worktree → 起库 → 执行器 → 保护路径检查 →
              卡口验证 → 推分支开 PR
ai-gates.sh   eng/04 §四 卡口清单的本地等价（逐条对照 ../../.github/workflows/ci.yml）
ai-loop.sh    无人值守入口：每调用跑一个任务即退，节奏交给 cron / systemd timer
check-regressions.py 驱动回归检查：临时 Git 仓库、提示词、环境与卡口编排
refs/         上游纪律事实源与台账（全文入库 + commit 锚点，见 refs/README.md）
ai-driver-logs/ Git common dir 同级的运行目录：提示词、执行器输出、卡口日志、PR 正文（不入库，见下）
```

## 快速上手

```bash
scripts/ai-driver/ai-run.sh --dry-run                      # 取第一个 pending 任务，只生成提示词
scripts/ai-driver/ai-run.sh --task T-0001-租约repo层与抢占测试
scripts/ai-driver/ai-run.sh --executor claude --no-pr      # 换执行器、不开 PR 只看结果
scripts/ai-driver/ai-loop.sh --max 1 --executor codex      # cron/systemd 的入口
scripts/ai-driver/ai-loop.sh --from 22:00 --until 08:00  # 跨午夜时间窗
```

依赖：`docker`（compose 起库）、`psql 17`、`gh`（开 PR）、执行器
`codex`（Codex CLI，已装 0.152）或 `claude`（Claude Code，已装 2.1.280）。
任务队列在 `../../docs/plan/backlog/`，格式见那里的 `README.md`。

独立运行全量卡口时，按 `AGENTS.md` §二设置 `DATABASE_URL` 与
`TGM_DB_URL_OWNER` 后运行 `scripts/ai-driver/ai-gates.sh`。测试控制连接不用于
SQLx schema 检查；后者默认使用 `app_user@tgm_test_schema`，初始化使用该库的
owner 连接。可用 `TGM_DB_URL_TEST`、`TGM_DB_URL_TASK_SCHEMA`、
`TGM_DB_URL_TASK_SCHEMA_OWNER` 覆盖，兼容对应的 `TGM_TEST_DATABASE_URL`、
`TGM_TASK_SCHEMA_DATABASE_URL`、`TGM_TASK_SCHEMA_OWNER_DATABASE_URL`，前者优先。

全量卡口先完成可信生产库迁移，再运行依赖常驻库的测试；schema 库在 SQLx 检查前
单独初始化。`--task-only` 不迁移生产库，须先完成可信迁移并保证 PgBouncer 可用；
`--from` 只运行指定卡口及之后的选中卡口，不能代替全量验收。

默认驱动回归已纳入本地和 CI static，失败必须阻断后续步骤。PR 引用使用当前 gh
支持的 GraphQL 查询，严格比较 base 名称、base SHA 和 head SHA；空结果或查询失败
拒绝完成。OpenAPI 接入在依赖实现之后由 PRE-004 负责，不在本分支提前启用。

驱动回归检查不调用模型、不推送分支、不创建 PR：

```bash
python3 scripts/ai-driver/check-regressions.py
python3 scripts/ai-driver/check-regressions.py --systemd --fresh-db
```

默认只运行临时文件与命令探针检查；`--systemd` 另需可用的用户 systemd 会话与
Rust/Node 工具链，使用真实服务检查环境白名单。`--fresh-db` 另需 Docker 和
`target/ai-gates-trusted/debug/tgm`（可先运行可信迁移卡口生成），用独立容器网络
验证空库迁移后运行 workspace 测试二进制，不占用宿主数据库端口、不重建开发容器。
此空库检查只验证迁移与测试顺序，不代替全量卡口的 SQLx、幂等和审计验收。

## 一轮的完整流程（ai-run）

```text
1 选取   --task 指定；不指定则取第一个 status: pending（文件名排序）
2 开荒   git worktree add，基于 cloud/dev（不是本地 dev：远端可能领先）
         分支名 codex/<任务id>，撞远端名自动 -r2 -r3；
         本地同名分支有未推送提交时拒绝重建（不静默覆盖）
3 起库   主仓目录 docker compose up -d --wait（worktree 不另起，撞端口）；
         执行器启动前确认 PG 可达；卡口阶段不再重新读取主仓 compose.yaml，
         避免执行器改主仓配置后影响验收
         执行器与卡口分别使用 Git common dir 下的隔离 target/，不共享主仓缓存；
         static 卡口不注入数据库连接，task 卡口的 sqlx-check 使用
         app_user@tgm_test_schema、测试使用 tgm_test@tgm_test_control，
         trusted 卡口才注入 owner 连接串
4 执行   提示词 = 任务文件全文 + AGENTS.md 指引 + 升级边界；
         执行器在自己的 worktree 里全权限跑（--dangerously-*），有超时；
         通过 systemd-run --user 的独立 control group 启动，超时或退出时回收整组进程；
         服务内部用 env -i 注入白名单，临时 HOME 与隔离 target 在实际服务中生效，
         不依赖 systemd 客户端环境继承；执行器默认没有数据库连接串，
         只有显式设置 TGM_DB_URL_EXECUTOR 才注入该连接，不注入 owner 连接串；
         Git 全局/系统配置、SSH 会话与 agent 运行时配置被隔离。
         环境清理不是文件系统沙箱：同一用户的全权限执行器仍可访问宿主文件
         未提交残局用 `stash create -u` 快照到本地 ref（不推远端）——
         纯未跟踪残局 `stash create -u` 输出为空（实测），兜底是全部入暂存区
         后不带 -u 再造一次；**残局本身留在 worktree**：卡口照它跑，
         保护路径检查看得见它
5 验证   先跑保护路径检查（PROTECTED 清单 + 任务声明的 allow-paths），
         以启动时钉死的 base SHA 直接比较保护路径的提交对象原始字节、文件类型与
         执行位，并补查提交树差异和磁盘新增路径 —— 「提交掉再还原工作区」与
         filter、属性文件、忽略规则都不能把改动藏掉。命中即强制升级、
         **不跑卡口** —— 卡口脚本本身在保护清单里，被改过之后跑出来的绿不携带
         信息。这是 AGENTS.md §五 硬边界的
         机器判据，不依赖提示词自觉。清单还含 AGENTS.md / CLAUDE.md /
         backlog：下一轮提示词由它们拼成，agent 改升级条件或改自己任务的
         escalation 就是自我减刑。检查本身 fail-loud：git 报错即中止，不放行。
         执行器启动前还要求主仓保护路径干净，并在执行器前后比较主仓保护路径的
         原始字节、HEAD 与索引快照，防止执行器用绝对路径改写主 checkout。
         检查之前还扫三个 git 视图之外的持久化形状：索引隐藏标志
         （skip-worktree / assume-unchanged，会让 Git 视图失明）、worktree 的
         Git 元数据指针与
         git 配置层指纹（.git/config、.git/info/attributes、全局配置与生效 hooks 目录 —— 都让执行器能让
         git「说谎」，且不在任何树视图里）—— 命中即同样强制升级、不跑卡口。
         卡口本身跑在 worktree 里（要读任务源码），但脚本内容从启动时钉死的
         基线 SHA 提交对象流式执行，不跑 agent 可改的主仓工作区副本；所有
         保护路径比较也固定到同一 SHA，并在卡口前后确认远端 base 未漂移。
         三个 helper
         脚本执行前用 `trusted_helper` 与基线版本逐**原始字节**比对（不走
         `git hash-object` —— 它会应用 clean filter，而 filter 正是配置层
         里能骗过哈希的通道）：不一致即拒绝，卡口不跑 agent 控制下的判据脚本。
         然后驱动亲自跑 ai-gates.sh，不信 agent 自述的 [report]；
         红则把失败尾部写入独立的回合提示词，保留旧提示词；生成失败不启动执行器，
         最多 --max-retries（默认 2）轮修复回合
6 出口   保护路径命中 → 推分支 + 开升级 PR，状态 escalated（标题明示未跑卡口）
         escalation: yes 必须有已提交提案，否则 blocked；有提案时强制 escalated
         path: ok 且卡口绿 且分支相对基线有提交 → 推分支 + 开 PR，状态 done
         自述 ok 但零提交 → 不推分支，状态 blocked（卡口跑在基线上必绿，
           这个绿不携带信息 —— 判据是 rev-list --count，不是 agent 自述）
         自述 ok 但工作区不干净 → 推分支保现场，状态 blocked：
           卡口验证的是工作区，推送的是提交树，两棵树不一致时绿不携带
           推送物的信息（工作区干净 ⇔ 验证的树 = 推送的树）
         path: escalated 或发现提案文件 → 推分支 + 开提案 PR，状态 escalated
         PR 创建失败（即使分支已推）→ 状态 blocked；其余 → 推分支保住现场，
         状态 blocked，PR 标题明示勿合；`--no-pr` 只推分支、不创建 PR，
         因此即使推送成功也只能是 blocked
7 回写   任务状态写入 Git common dir 下的 ai-driver-state/<任务>.state，
         backlog 只保存静态任务定义，不再由驱动制造保护路径脏状态；
         --commit-status 已废弃，状态不提交到 dev，也不进入 PR 冲突面。
```

## 人保留的三个动作

```text
1 合并 PR —— 驱动永不合并；卡口绿的 PR 也只在 CI 绿之后由人点合并
2 裁决升级 —— proposals/ 里的提案：落回 backlog 或改文档，都在人手里
3 定任务与改判据 —— backlog 由人写；判据变更属于基线变更，走 plan/00 §九
```

## 已知边界（照仓库惯例写明，不当作已解决）

```text
1 本地绿 ≠ 可合并。eng/04 §六 记过多个「本地全绿、CI 红」的实例
  （pg_dump 版本、runner 环境差异）。裁决永远在 CI 那一次真跑。
2 卡口反证（双遍脚本）的完整性由保护路径检查兜底：agent 改动
  scripts/ 等保护路径时不跑卡口直接升级。兜不住的是**基线里本来就坏的脚本**
  —— 所以 PROTECTED 清单本身只有人能改（见 AGENTS.md §七 信任边界）。
  另外 .sqlx 那条卡口的反证仍会被增量编译骗过（eng/04 §六），驱动不重复反证。
3 tgm openapi --check 刻意不跑：WP-5 前它 exit 1，红不携带信息（同 ci.yml）。
4 驱动不并行多任务：入口用 Git common dir 下的 `flock` 互斥锁保证一次只有
   一个 `ai-run`，一个库、一条卡口序列；各任务使用隔离 target/，并行仍会把失败方向搅混。
   要吞吐先换「人审合并」的吞吐，不要在这里换。
5 执行器与卡口各有超时 watchdog，超时会终止各自 systemd service 的整个 cgroup。
   执行器被杀后驱动仍按产出判定；符合后续检查条件才跑卡口，不能把超时本身算通过。
6 运行日志位于 Git common dir 同级的 `ai-driver-logs/`，不进入仓库：
   它含完整执行器对话，量大且含环境细节；要留证就摘进 PR 正文。
7 本地工具链与 CI 的版本漂移：ci.yml 钉 `cargo-deny 0.19.1`、`sqlx-cli 0.9.0`，
  本地装什么跑什么（实测本机 cargo-deny 0.20.2）。漂移是「本地绿 / CI 红」
  的一条新来源，与边界 1 同源；要根治就在本地也按 ci.yml 的版本装。
8 保护路径检查固定对启动时的 base SHA，直接比较磁盘内容与提交对象，
  再补查提交树差异和新增路径；判据是「任务改没改」，
  不是「清单里的文件现在对不对」—— 后者由 CI 的卡口自身保证。
  任务确需触碰时用任务文件的 `allow-paths:` 放行，放行责任在写任务的人。
  `.cargo/` 在清单里：rustc-wrapper 等配置等于换掉编译器本身。
  `build.rs` 刻意不在：它是被测物（编译期运行、进 PR diff 可见），
  圈进来会让每个加 crate 的任务都误升级。
9 清单刻意不含 docs/plan/proposals/ 与 docs/plan/ponytail-debt.md：提示词
  点名要 agent 写这两处（升级提案、§4.4 债务登记）。把 docs/plan/ 整个圈进来
  会让每个正常任务都误判 protected，并且把 escalated 这条出口整个废掉。
  粒度只到 backlog/ —— 那里才是「下一轮提示词的输入」。
10 视图之外的四道补充防线：
  （a）索引隐藏标志（skip-worktree / assume-unchanged）会让 Git 视图与
  卡口后的脏树判据全部失明（实测）。新鲜 worktree 里没有合法来源，
  扫到即强制升级；
  （b）worktree 的 `.git` 指针与 common dir 元数据被锁定，改动即强制升级；
  （c）git 配置层指纹：执行器让 git「说谎」的通道全在配置层（.git/config、
  .git/info/attributes、全局配置与生效 hooks 目录），树检查看不见。实测两条：
    · clean/smudge filter 会让 `git hash-object` 与工作区 diff 对磁盘恶意
      内容吐出良性字节（配一个吐出基线内容的 filter，旧版 `trusted_helper`
      放行了恒绿恶意脚本）；
    · `core.hooksPath` 重定向把 hooks 生效目录指到别处，只比旧目录会漏。
  处置：执行器跑之前对「本地配置 + 生效 hooks 目录的类型、权限、链接目标与
  内容（含链接目标文件摘要）」取指纹（生效目录每轮重新解析），变了即强制升级。
  系统/全局配置也纳入运行前后指纹；执行器环境将其置空，变化即升级。
  （d）远端 base 在启动、卡口前后与推送前后都必须仍指向启动 SHA；推送只写
  任务分支，并用空值 lease 防止并发占用同名分支，绝不把受保护 base 当作
  push ref。
11 零提交检查只看「有没有提交」，不看提交里是什么。agent 提交一个空壳文件
  照样过这一条，真正拦它的是任务判据与卡口。这条只堵「一行没写也判 done」。
```

## 与文档体系的关系

本目录不定义任何设计结论。卡口的定义处是 `eng/04`，任务判据的定义处是
`plan/00` §4.3 与 `eng/03`，升级条件的定义处是仓库根 `AGENTS.md` §五。
这里与它们不一致时，以它们为准，并回来改这里。
