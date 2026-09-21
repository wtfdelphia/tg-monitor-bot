# ADR-0020：单二进制多 subcommand，四个 crate

状态：已定案

## 决定

`spec/02` 划了 7 个组件，但代码不做成 7 个 crate、也不做成 7 个 bin。

```text
crates/core  领域类型：newtype、错误、配置、AuthContext
crates/db    sqlx、tenant_tx、Repo trait 与实现、migrations
crates/tg    grammers + teloxide 封装，raw invoke 收敛在此
crates/app   全部业务逻辑
bins/tgm     单二进制 + subcommand
```

## 背景

组件划分是部署与职责的划分，不是编译单元的划分。把两者一一对应
是常见的直觉错误，代价是每个 crate 边界都要搬一轮类型。

选单二进制的三个理由：

- 首发锁定的部署形态就是单机跑多进程（[[ADR-0025]]），
  subcommand 天然支持 —— `tgm serve --role X`。
- 共享编译产物，CI 时间减半。Rust 的编译时间是真实成本（[[ADR-0002]]）。
- 关键路径不可压缩，少一个 crate 边界就少一轮类型搬运。

`tg/` 单独成 crate 的理由不是整洁，是**依赖退出路径**：
grammers 是 0.x 且 bus factor 低，把 raw invoke 收敛在一个 crate 里，
将来 fork 或替换时改动面可控。

`db/` 单独成 crate 的理由类似但方向不同：`tenant_tx` 与 Repo trait 是
租户隔离在应用层的唯一入口，边界越窄越容易审。

## 代价

- 单二进制体积包含所有角色的代码。跑 `--role delivery` 的进程里
  也链着控制台的 axum 路由，攻击面比拆开大。
  缓解是配置层面的：`control-plane` 拿不到 KEK 配置就不可能解密
  （[[ADR-0011]]），能力边界靠配置而不靠二进制边界。
- `crates/app` 会长得很大，内部模块划分要自律，编译器不帮忙。
- 将来要拆成多进程独立部署时，crate 划分需要重做 ——
  这是有意接受的：现在拆是为假想需求付钱。

## 相关

[[ADR-0002]]、[[ADR-0010]]、[[ADR-0011]]、[[ADR-0025]]。
落地在 `eng/00-工程约定.md`（crate 布局）、`ops/00-部署与运行时.md`。
