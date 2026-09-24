---
name: caveman-commit
description: >
  Write a Conventional Commits message compressed to intent only. Use for
  "write a commit", "commit message", /commit or /caveman-commit.
---

Write commit messages terse and exact. Conventional Commits format. No fluff. Why over what.

## Rules

**Subject line:**
- `<type>(<scope>): <imperative summary>` — `<scope>` optional
- Types: `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `chore`, `build`, `ci`, `style`, `revert`
- Imperative mood: "add", "fix", "remove" — not "added", "adds", "adding"
- ≤50 chars when possible, hard cap 72
- No trailing period
- Match project convention for capitalization after the colon

**Body (only if needed):**
- Skip entirely when subject is self-explanatory
- Add body only for: non-obvious *why*, breaking changes, migration notes, linked issues
- Wrap at 72 chars
- Bullets `-` not `*`
- Reference issues/PRs at end: `Closes #42`, `Refs #17`

**What NEVER goes in:**
- "This commit does X", "I", "we", "now", "currently" — the diff says what
- "As requested by..." — use Co-authored-by trailer
- "Generated with Claude Code" or any AI attribution — unless the user's own rule requires an `Assisted-by`/AI-attribution trailer, then add it as a trailer
- Emoji (unless project convention requires)
- Restating the file name when scope already says it

## Examples

Diff: new endpoint for user profile with body explaining the why
- ❌ "feat: add a new endpoint to get user profile information from the database"
- ✅
  ```
  feat(api): add GET /users/:id/profile

  Mobile client needs profile data without the full user payload
  to reduce LTE bandwidth on cold-launch screens.

  Closes #128
  ```

Diff: breaking API change
- ✅
  ```
  feat(api)!: rename /v1/orders to /v1/checkout

  BREAKING CHANGE: clients on /v1/orders must migrate to /v1/checkout
  before 2026-06-01. Old route returns 410 after that date.
  ```

## Auto-Clarity

Always include body for: breaking changes, security fixes, data migrations, anything reverting a prior commit. Never compress these into subject-only — future debuggers need the context.

## Boundaries

Only generates the commit message. Does not run `git commit`, does not stage files, does not amend. Output the message as a code block ready to paste. "stop caveman-commit" or "normal mode": revert to verbose commit style.

---

## 本地出处说明

- 来源：JuliusBrussee/caveman 的 `skills/caveman-commit/SKILL.md`，
  commit `2fd153c67988e980fb0b2455c90832159a6a5a25`，2026-09-23 拉取。上游 MIT。
- 角色：仓库根 `AGENTS.md` §6.2 提交纪律的事实源。上游正文保持英文原文。
- **已知分叉**（登记于本节，不在别处）：
  1 上游要求英文祈使句摘要；本仓 subject/body 用中文书写，冲突时以本仓为准。
  2 上游「NEVER 写 AI attribution」；本仓同样不写「AI 生成」尾注，一致。
  3 上游 body 触发条件（why/breaking/migration/issue）之外，本仓另有高风险域
    必须写 body 的清单（见 `AGENTS.md` §6.2）。
- 更新方式：重拉上游 `SKILL.md` 替换上方正文，重核分叉清单，保留本节。
