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
refs/         上游纪律事实源与台账（全文入库 + commit 锚点，见 refs/README.md）
logs/         每次运行的提示词、执行器输出、卡口日志、PR 正文（不入库，见下）
```

## 快速上手

```bash
scripts/ai-driver/ai-run.sh --dry-run                      # 取第一个 pending 任务，只生成提示词
scripts/ai-driver/ai-run.sh --task T-0001-租约repo层与抢占测试
scripts/ai-driver/ai-run.sh --executor claude --no-pr      # 换执行器、不开 PR 只看结果
scripts/ai-driver/ai-loop.sh --max 1 --executor codex      # cron/systemd 的入口
```

依赖：`docker`（compose 起库）、`psql 17`、`gh`（开 PR）、执行器
`codex`（Codex CLI，已装 0.152）或 `claude`（Claude Code，已装 2.1.280）。
任务队列在 `../../docs/plan/backlog/`，格式见那里的 `README.md`。

## 一轮的完整流程（ai-run）

```text
1 选取   --task 指定；不指定则取第一个 status: pending（文件名排序）
2 开荒   git worktree add，基于 cloud/dev（不是本地 dev：远端可能领先）
         分支名 codex/<任务id>，撞远端名自动 -r2 -r3；
         本地同名分支有未推送提交时拒绝重建（不静默覆盖）
3 起库   主仓目录 docker compose up -d --wait（worktree 不另起，撞端口）；
         worktree 共享主仓 target/（CARGO_TARGET_DIR），一次一个任务所以安全
4 执行   提示词 = 任务文件全文 + AGENTS.md 指引 + 升级边界；
         执行器在自己的 worktree 里全权限跑（--dangerously-*），有超时；
         未提交残局用 `stash create -u` 快照到本地 ref（不推远端）——
         纯未跟踪残局 `stash create -u` 输出为空（实测），兜底是全部入暂存区
         后不带 -u 再造一次；**残局本身留在 worktree**：卡口照它跑，
         保护路径检查看得见它
5 验证   先跑保护路径检查（PROTECTED 清单 + 任务声明的 allow-paths），
         对基线取三条来源的并集：工作区已跟踪改动、提交树（HEAD）、
         未跟踪新文件 —— 「提交掉再还原工作区」只骗得过第一条：
         命中即强制升级、**不跑卡口** —— 卡口脚本本身在保护清单里，
         被改过之后跑出来的绿不携带信息。这是 AGENTS.md §五 硬边界的
         机器判据，不依赖提示词自觉。清单还含 AGENTS.md / CLAUDE.md /
         backlog：下一轮提示词由它们拼成，agent 改升级条件或改自己任务的
         escalation 就是自我减刑。检查本身 fail-loud：git 报错即中止，不放行。
         然后驱动亲自跑 ai-gates.sh，不信 agent 自述的 [report]；
         红则把失败尾部塞回提示词，最多 --max-retries（默认 2）轮修复回合
6 出口   保护路径命中 → 推分支 + 开升级 PR，状态 escalated（标题明示未跑卡口）
         path: ok 且卡口绿 且分支相对基线有提交 → 推分支 + 开 PR，状态 done
         自述 ok 但零提交 → 不推分支，状态 blocked（卡口跑在基线上必绿，
           这个绿不携带信息 —— 判据是 rev-list --count，不是 agent 自述）
         自述 ok 但工作区不干净 → 推分支保现场，状态 blocked：
           卡口验证的是工作区，推送的是提交树，两棵树不一致时绿不携带
           推送物的信息（工作区干净 ⇔ 验证的树 = 推送的树）
         path: escalated 或发现提案文件 → 推分支 + 开提案 PR，状态 escalated
         其余 → 推分支保住现场，状态 blocked，PR 标题明示勿合
7 回写   任务文件 status/last-run 在**主仓的 base 分支**原地更新
         （加 --commit-status 会顺手提交，限定路径只提交任务文件）。
         记账不落在任务分支上：backlog 在保护清单里，agent 不碰任务文件，
         所以合并不会撞；反过来把状态写进未合并的分支，主仓的取件看不见，
         同一个任务会被反复取件。
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
4 驱动不并行多任务：一个库、一个 target/、一条卡口序列，并行会把失败方向搅混。
   要吞吐先换「人审合并」的吞吐，不要在这里换。
5 --timeout 杀掉的是执行器进程；卡口被杀不会发生（卡口在循环里，不在超时里）。
   执行器被杀后驱动仍按产出判定并跑卡口 —— 半成品会以 blocked 暴露，不会静默绿。
6 logs/ 不入库（../../.gitignore 已加 scripts/ai-driver/logs/）：
   它含完整执行器对话，量大且含环境细节；要留证就摘进 PR 正文。
7 本地工具链与 CI 的版本漂移：ci.yml 钉 `cargo-deny 0.19.1`、`sqlx-cli 0.9.0`，
  本地装什么跑什么（实测本机 cargo-deny 0.20.2）。漂移是「本地绿 / CI 红」
  的一条新来源，与边界 1 同源；要根治就在本地也按 ci.yml 的版本装。
8 保护路径检查是对基线的 diff，三条来源并集（工作区、提交树、未跟踪），
  判据是「任务改没改」，
  不是「清单里的文件现在对不对」—— 后者由 CI 的卡口自身保证。
  任务确需触碰时用任务文件的 `allow-paths:` 放行，放行责任在写任务的人。
9 清单刻意不含 docs/plan/proposals/ 与 docs/plan/ponytail-debt.md：提示词
  点名要 agent 写这两处（升级提案、§4.4 债务登记）。把 docs/plan/ 整个圈进来
  会让每个正常任务都误判 protected，并且把 escalated 这条出口整个废掉。
  粒度只到 backlog/ —— 那里才是「下一轮提示词的输入」。
10 零提交检查只看「有没有提交」，不看提交里是什么。agent 提交一个空壳文件
  照样过这一条，真正拦它的是任务判据与卡口。这条只堵「一行没写也判 done」。
```

## 与文档体系的关系

本目录不定义任何设计结论。卡口的定义处是 `eng/04`，任务判据的定义处是
`plan/00` §4.3 与 `eng/03`，升级条件的定义处是仓库根 `AGENTS.md` §五。
这里与它们不一致时，以它们为准，并回来改这里。
