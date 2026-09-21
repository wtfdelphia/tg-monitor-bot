# ADR-0007：`tenant_id` 用 `BIGINT` 且一律 NOT NULL

状态：已定案

## 决定

`tenant_id BIGINT NOT NULL`。不用 UUID，不用 TEXT slug。
会话变量侧一律 `current_setting('app.tenant_id')::bigint`。

NOT NULL 的例外只有三张表：`identities`（[[ADR-0014]] 的演示源）、
`system_logs`、`audit_logs`（后两张记录平台级事件，本来就无租户归属）。

## 背景

UUID 的吸引力是「id 不可枚举」。但在这个系统里枚举防护不是靠 id 不可猜 ——
`tenant_id` 从不由客户端传入，它来自 `AuthContext`，
猜到别人的 id 也没有任何用处。于是 UUID 只剩下代价：
索引宽度翻倍，而所有查询索引都以 `tenant_id` 打头（铁律二），
这个宽度乘在每一条索引上。

`BIGINT` 还带来一个安全侧的好处：`::bigint` 转换会拒绝空串。
这不是写法偏好而是机制 —— PgBouncer 执行 `DISCARD ALL` 后
`app.tenant_id` 是空串而不是 NULL，
`::bigint` 让它炸成 `invalid input syntax for type bigint: ""`
而不是被静默当成某个值（见 [[ADR-0005]]、[[ADR-0010]]）。

NOT NULL 之所以是铁律而不是习惯，是因为复合外键依赖它：
`MATCH SIMPLE` 下任一列为 NULL 则整个外键检查被跳过，
`tenant_id` 可空会让 [[ADR-0006]] 的复合外键完全失效。

## 代价

- id 出现在 URL 与日志里就是连续的整数，能看出租户数量与增长速度。
  这是接受的信息泄漏 —— 它不导致越权。
- 三张例外表要在静态断言 A5 里硬编码排除。白名单会腐烂，
  所以 `eng/04` 明写了「**不要为新表扩那个白名单**」。
- `identities` 那处例外的完整代价见 [[ADR-0006]]。

## 相关

[[ADR-0005]]、[[ADR-0006]]、[[ADR-0010]]、[[ADR-0014]]。
落地在 `spec/03-数据模型.md`、`eng/04-CI门禁.md`（A5）。
