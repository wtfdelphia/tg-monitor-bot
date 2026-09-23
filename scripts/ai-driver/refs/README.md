# refs —— 上游纪律的事实源与台账

`AGENTS.md` §4（ponytail）、§6.1（humanizer-zh）、§6.2（caveman-commit）引用
的上游规则全文。三个上游处理对齐：**全文入库 + commit 锚点 + 拉取日期 +
本地出处节**。上游改纪律时，锚点让过期可发现；重拉后逐字节比对再更新。

## 台账

| 文件 | 上游 | 锚点（拉取 2026-09-23） | 许可 | 本仓适配 |
|---|---|---|---|---|
| `ponytail.md` | DietrichGebert/ponytail `.openclaw/skills/ponytail/SKILL.md` | `e3ba2aa` | MIT | §4.1 六域清单是本仓版：上游无「模型执行」，以信封加密与凭据替入（WP 阶段的临时替换 —— `plan/00` §6.4 演进期候选项里有 AI 语义过滤，届时回来重核本节） |
| `caveman-commit.md` | JuliusBrussee/caveman `skills/caveman-commit/SKILL.md` | `2fd153c` | MIT | 已分叉，逐条登记在文件的本地出处节（中文书写、高风险域 body 清单） |
| `humanizer-zh.md` | op7418/Humanizer-zh `SKILL.md` | `f4518a8` | MIT | 冲突仲裁：与本仓工程约定冲突时以本仓为准（见 `AGENTS.md` §6.1） |

## 纪律

```text
1 上游正文保持原文（英文或中文），不翻译、不改写 —— 改了就失去「重拉比对」的意义
2 本仓适配与分叉只写在「本地出处说明」节，不掺进正文
3 更新流程：重拉对应路径 → 与库内版本逐字节比对 → 有差异则替换正文、
   重核出处节的适配与分叉清单、在任务或提交里记录差异
```
