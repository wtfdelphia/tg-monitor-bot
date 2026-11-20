# ADR-0002：用 Rust 实现，不用 Python

状态：已定案

## 决定

全栈 Rust（axum + sqlx + tokio + grammers + teloxide），
不用 Python（Telethon + FastAPI）。

## 背景

Python 在这个领域是默认选项，而且优势是真的：
Telethon 成熟度远高于 grammers，同类开源项目（tgcf、TelegramForwarder）
全是 Python，参考实现随手可得。

选 Rust 的理由只有一条站得住：**这个系统的核心风险是隔离，
而隔离错误在 Python 里是运行时错误，在 Rust 里能做成编译期错误。**

具体形式：`TenantId` / `ChatId` / `IdentityId` 全是 newtype
（`spec/03` 的一名三义纪律直接来自 `chat_id` 混用），
`AuthContext` 是每个 Repo 方法的必需参数，
忘记传 `tenant_id` 编译不过。Python 里同样的保证要靠测试覆盖，
而测试只能覆盖写过的路径。

次要理由：mtproto-worker 是常驻长连接进程，内存占用直接决定单机能挂多少租户
（`ops/00` 的容量口径）；Python 的 GIL 在这个形态下要靠多进程绕，
反而把连接数放大。

## 代价

**这条决策买来的是编译期约束，付出的是生态成熟度。** 具体：

- grammers 是 0.x，bus factor 低，而且仓库搬过家（GitHub 那份已归档，
  活的在 Codeberg）。QR 登录、`SendCode` 等关键路径要靠 raw invoke 自己拼。
  缓冲手段是把 raw invoke 收敛在 `tg/` 一个 crate 里，
  让「换库或 fork」时的改动面可控（`eng/00` 的 crate 布局）。
- 迭代速度慢。对 UI 与运营脚本这类变化频繁的部分，Rust 是纯负担 ——
  这也是控制台走服务端渲染而没有前端工程的原因之一。
- 招人面变窄。
- `spec/08` 里所有 grammers 用法**都是读源码得出的，没跑过**。
  这部分风险在动工前验证阶段暴露，不在首发。

## 相关

[[ADR-0001]]、[[ADR-0020]]。
落地在 `eng/00-工程约定.md`、`spec/08-账号接入.md`。
