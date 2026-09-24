# backlog —— AI 驱动循环的任务队列

驱动（`../../../scripts/ai-driver/`）从这里取任务：文件名 `T-*.md`，按文件名排序
取第一个 `status: pending` 的。一任务一文件，YAML-ish 头部 + 正文。

## 格式

```markdown
# T-0000：<标题>

status: pending | active | done | escalated | blocked
wp: <所属工作包，如 WP-2>
escalation: no | yes        # yes = 预期会命中 AGENTS.md §五 升级条件
allow-paths: <逗号分隔，可选> # 任务确需触碰保护路径时的放行清单；
                            # 保护路径清单见 scripts/ai-driver/ai-run.sh 的 PROTECTED。
                            # 放行责任在写任务的人，默认不写。
criteria: <验收判据出处的引用，如 plan/00 §4.3 / eng/03 R17>

<正文：做什么、判据原文摘录、边界。引用唯一出处，不复制数字>
```

## 纪律

```text
1 每条任务必须带机器可核的验收判据（卡口可判定的形状）——
  没有判据的任务不写进来（同 pre-do/README.md 纪律 2）
2 不复制工期、表数、断言条数等数字 —— 引用唯一出处（docs/design/README.md 硬规则 1）
3 命中升级条件的任务标记 escalation: yes 而不是不写 ——
  驱动会让 agent 产出提案而不是硬闯
4 status 由人维护在任务文件；驱动把运行状态与 last-run 写入 Git common dir
  下的本地账本，不改 backlog，也不接受已废弃的 --commit-status 参数。
  人手工改 status 后，删除对应本地账本即可让任务重新取件。
  `--no-pr` 只允许保留 `blocked`，不能把未创建 PR 的分支记为 `done`。
5 本目录在驱动的保护清单里（`scripts/ai-driver/ai-run.sh` 的 PROTECTED）：
  执行器改任何任务文件都会被判升级。理由有两条 —— 任务文件是下一轮提示词的
  输入（改自己的 escalation 就是自我减刑），以及 backlog 是静态任务定义；
  运行状态不再与任务文件发生双写冲突
6 卡口的数据库边界分层：static 不连接数据库；task 的 `sqlx-check` 使用
  `app_user` 连接测试 schema 库，测试使用 `tgm_test` 连接控制库；trusted
  阶段才使用 owner 连接串。backlog 只定义任务，不承载这些运行时连接配置。
```

## 排序建议

任务 id 即优先级：同一工作包内按依赖排序，跨包不穿插
（`plan/00` §4.1：包内不通过就不进下一个包）。

## 相关工件

- 任务归档后，实现里留下的 `ponytail:` 注释登记进 `../ponytail-debt.md`
  （纪律见仓库根 `AGENTS.md` §4.4）。
- 任务文件、提案与交付的中文散文过 `../../scripts/ai-driver/refs/humanizer-zh.md` 的润色纪律
  （见 `AGENTS.md` §6.1）；本文件里的判据引用不复制数字。
