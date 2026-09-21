# ADR-0008：规则用关系表，不用 JSONLogic

状态：已定案

## 决定

规则条件存成关系表（`rules` + `rule_conditions` + `rule_replacements`），
不存成一列 JSONB 里的 JSONLogic 表达式。两者不并存。

## 背景

JSONLogic 的优势是表达力：任意嵌套的 and / or / not，
加新操作符不用改表结构。同类开源项目多数走这条路。

选关系表的理由是**可校验性**：

- 规则冲突（同一源订阅上两条规则互相矛盾、优先级相同）能用一条 SQL 查出来。
  JSONB 里要靠应用层反序列化后逐条比对。
- 命中路径能用索引。`rule_conditions (rule_id)` 是查询热点。
- 迁移可控：加一种条件类型是一次 `ALTER TABLE` 加一个枚举值，
  改动可见、可审计。JSONB 的 schema 变更是隐式的，
  老数据和新代码的兼容关系没有任何机制记录。

而任意嵌套在首发用不上 —— 首发的规则语义是「条件全部满足则命中」，
扁平的 AND 列表足够。

顺带一个相关但独立的取舍：`rule_replacements` 带 `tenant_id`
而 `rule_conditions` 不带，是因为前者的写入路径是整体替换
（`DELETE ... WHERE rule_id = $1`，不经父表），
有本表自己的 `tenant_id` 才能套甲类 RLS 模板而不必写 EXISTS 子查询。

## 代价

- 加一种条件类型要改表，比往 JSON 里加个 key 慢。
- 嵌套逻辑做不到。真需要时不能「顺手加个字段」——
  要写新 ADR 取代本条，并且那是一次数据迁移。
- `rule_conditions` 没有 `tenant_id`，属于乙类 RLS 表，
  policy 要靠 EXISTS 子查询，这是 `eng/04` 主查询覆盖不到的已知边界，
  需要一条用白名单表名的补充断言 —— 全库唯一一处硬编码表名的断言。

## 相关

[[ADR-0003]]、[[ADR-0009]]。
落地在 `spec/03-数据模型.md`（规则四表）、`eng/04-CI门禁.md`。
