# 修复 0b19db 深度审核问题

## 任务

修复提交 `0b19dbf1989db0bac2e64b0aa12dfabab5fdc17f` 引入的 AI 驱动循环在
数据库权限、执行器隔离、Git 信任根、卡口生命周期、Compose 配置、日志与状态账本、
任务出口方面的问题。

本次改动命中 `AGENTS.md` §五 的安全面、卡口与发布边界，先提交提案，待人审核后再
修改驱动实现、卡口脚本和相关验收检查。

## 已确认问题

### P0

1. `run_gates` 在任务 worktree 中执行卡口，同时注入 `TGM_DB_URL_OWNER`。任务树里的
   `build.rs`、测试、过程宏或二进制可以读取 owner DSN，访问不属于任务代码的高权限
   数据库能力。位置：`scripts/ai-driver/ai-run.sh:731-744`、
   `scripts/ai-driver/ai-gates.sh:35-38,133-158`。
2. `executor_env` 显式继承宿主 `HOME` 和 `PATH`。执行器可读取宿主 GitHub、SSH、
   Cargo、云厂商配置，或通过用户可写 PATH 影响工具解析。位置：
   `scripts/ai-driver/ai-run.sh:617-643`。
3. 执行器 watchdog 只按初始进程组终止；执行器通过 `setsid`、double-fork 等方式脱离
   后，超时后仍可继续修改 worktree、主仓或其他路径。位置：
   `scripts/ai-driver/ai-run.sh:646-693`。
4. 驱动和卡口中的 Git 对象读取没有统一设置 `GIT_NO_REPLACE_OBJECTS=1`，也没有拒绝
   `refs/replace/*`。攻击者可用 replace ref 改写固定基线对象的解释，绕过保护路径和
   helper 完整性检查。位置：`scripts/ai-driver/ai-run.sh:526-535`、
   `scripts/ai-driver/ai-gates.sh:121-137`。

### P1

1. `--no-db --from migrate` 会跳过全部卡口并返回 0，最后仍输出“全部卡口通过”。已
   复现；位置：`scripts/ai-driver/ai-gates.sh:53-85,160-165`。
2. 独立运行 `ai-gates.sh --no-db` 时不清理调用环境中已有的 `DATABASE_URL` 与
   `TGM_DB_URL_OWNER`，构建期代码仍可读取这些凭据。位置：
   `scripts/ai-driver/ai-gates.sh:35-38`。
3. `--no-pr` 推送分支后返回成功，调用方随后把任务写成 `done`，但没有 PR 或人工合并
   入口。位置：`scripts/ai-driver/ai-run.sh:1141-1147,1180-1187`。
4. 推送后到 `gh pr create` 之间仍存在 base TOCTOU；没有读取实际 PR base SHA 并与启动时
   `BASE_COMMIT` 比较。位置：`scripts/ai-driver/ai-run.sh:1136-1169`。
5. 执行器可修改主仓 `.env`、Compose override 或外部 Compose 配置；后续卡口通过
   `TGM_COMPOSE_DIR="$PRIMARY"` 启动 Compose 时会加载这些配置，污染数据库和卡口环境。
   位置：`scripts/ai-driver/ai-run.sh:517-553`、`scripts/ai-driver/ai-gates.sh:87-98`。
6. 卡口阶段没有独立进程组、超时和后代清理。任务代码可让卡口永久阻塞，或留下后台
   进程在保护检查后继续修改文件。位置：`scripts/ai-driver/ai-run.sh:730-744`。
7. Git 配置 include 可指向 FIFO；`git config --local --includes --list` 没有超时，
   可让驱动永久阻塞并长期持有锁。位置：`scripts/ai-driver/ai-run.sh:801-809`。
8. “非零提交”检查只看提交数量。执行器可创建 `--allow-empty` 提交，分支相对基线有
   提交但 tree 不变，仍可能进入 `done`。位置：`scripts/ai-driver/ai-run.sh:1102-1117`。
9. `logs/` 位于主仓且被保护检查整体豁免；目录或日志文件被替换为符号链接时，驱动
   写提示词、执行器输出或 PR 正文可能覆盖外部路径。位置：
   `scripts/ai-driver/ai-run.sh:23,76-78,159-170,278-291`。

### P2

1. 缺少基线卡口脚本时直接从当前 `HEAD` bootstrap，没有仓库外固定信任锚。位置：
   `scripts/ai-driver/ai-run.sh:526-533`。
2. `state_fingerprint` 不包含状态根目录自身的类型、权限、属主和真实路径，根目录属性
   变化可能不被发现。位置：`scripts/ai-driver/ai-run.sh:235-272`。
3. 保护目录的 `find`、`stat`、读取和比较错误可能被重定向吞掉，部分不可遍历形状可能
   被当作没有差异。位置：`scripts/ai-driver/ai-run.sh:159-208`。
4. 已提交的提案符号链接会被当成有效提案：Git 条目类型为 blob，工作区的 `[ -f ]`
   会跟随链接。位置：`scripts/ai-driver/ai-run.sh:1078-1083`。
5. 重启任务时无条件删除旧 worktree，可能丢失人工检查中的残局；显式 `--task` 也不校验
   当前状态，已完成任务可以被重复执行。位置：`scripts/ai-driver/ai-run.sh:369-400,462-469`。

## 修复方案

### 1. 固定 Git 信任根

- 在驱动和卡口脚本中统一通过 `env GIT_NO_REPLACE_OBJECTS=1` 执行所有对象读取、树
  比较、helper 校验和 ref 解析命令。
- 启动时拒绝 `refs/replace/*` 及其他会改变对象解释的异常 Git 环境；把相关配置、引用
  和对象读取失败作为 fail-closed。
- 删除从当前 `HEAD` 无条件 bootstrap 的路径。基线不含可信卡口脚本时直接 blocked，
  或只允许仓库外固定 SHA/blob 摘要匹配的人工配置。

### 2. 拆分任务代码与高权限卡口

- 任务代码、构建脚本、测试、过程宏和普通 `cargo run` 不接触 owner DSN；`--no-db`
  必须清除所有数据库环境变量。
- 将需要高权限的迁移、RLS 审计和双遍测试拆为固定可信入口；任务代码只使用受限测试
  角色或无数据库环境。
- 为卡口增加反向测试：任务代码读取 owner DSN、执行 DDL、修改角色或越权读取租户时
  必须失败；受限测试角色不能访问生产对象。

### 3. 收紧执行器与卡口的进程边界

- 执行器使用临时专用 `HOME`、`XDG_CONFIG_HOME`、`CARGO_HOME` 和固定系统 `PATH`，
  不继承宿主用户目录与用户可写路径。
- 执行器和卡口均放入独立的进程/cgroup 边界；超时时终止整个边界，确认无后代进程后
  才进入保护检查、卡口和状态结算。
- 保留 fd 9 关闭逻辑，并验证所有内部 fd 不会被执行器继承。
- Git 配置读取必须拒绝 FIFO、设备文件和不可读对象，并使用有界超时，超时或读取失败
  一律 fail-closed。

### 4. 隔离 Compose、日志和状态账本

- 主仓 Compose 使用固定配置文件和清理后的环境，禁止 `.env`、override、
  `COMPOSE_FILE`、`COMPOSE_PROFILES` 等未审计输入影响卡口；相关文件纳入主仓守卫或
  由外部固定路径提供。
- 将运行日志移到 Git common dir 外的驱动专用目录，安全创建并拒绝符号链接；删除主仓
  `logs/` 的整体豁免。
- `state_fingerprint` 纳入状态根目录自身的类型、权限、属主、真实路径和链接状态；
  `check_state_dir` 校验目录权限、属主和可遍历性。
- 保护目录自身纳入权限、属主和可遍历性检查；所有扫描失败必须返回错误，不得视为空结果。

### 5. 修复任务与发布出口

- `--no-pr` 不得进入 `done`，改为明确的 `pushed` 或 `blocked` 状态，并同步更新 README
  与 backlog 语义。
- `--no-db` 与数据库起始卡口组合直接拒绝；输出区分“已执行卡口通过”和“跳过卡口”，
  不再输出无条件的“全部卡口通过”。
- 推送和创建 PR 后查询实际 PR 的 base SHA；与 `BASE_COMMIT` 不一致时标记 blocked，
  不写入 done。
- 除提交数量外比较 `BASE_COMMIT^{tree}` 与交付 HEAD 的 tree，拒绝空提交作为完成交付。
- 提案必须是已提交的普通文件，拒绝 Git symlink；旧 worktree 默认保留并要求显式人工
  清理，显式 `--task` 只允许 pending 或明确的人工重试状态。

## 验收判据

- `bash -n scripts/ai-driver/ai-run.sh scripts/ai-driver/ai-loop.sh scripts/ai-driver/ai-gates.sh`
- `git diff --check`
- `scripts/ai-driver/ai-gates.sh --no-db` 只执行非数据库卡口；与 `--from migrate` 组合
  非零；所有跳过项有明确汇总；`--no-db` 下数据库环境变量不可见。
- 受控 Git 仓库中设置 `refs/replace/*`、异常对象环境、缺少可信 bootstrap、FIFO include
  时，驱动拒绝运行或在有界时间内 fail-closed。
- 受控执行器不能读取宿主 `HOME`、SSH、GitHub、云厂商和 Cargo 凭据，不能释放驱动锁，
  执行器或卡口超时后不能留下可运行后代进程。
- 任务代码不能读取 owner DSN、执行高权限 DDL、读取其他租户或修改角色；受限测试角色
  的失败形状符合卡口判据。
- Compose 隐式配置、日志目录符号链接、状态根目录权限变化和保护目录权限变化均被拒绝。
- 空提交、提案符号链接、重复执行已完成任务、旧 worktree 证据删除、`--no-pr` 错记和
  PR base 漂移均有正反向测试。
- 完整运行 `scripts/ai-driver/ai-gates.sh`；最终以 CI 为裁决。

## 人工决策

请先确认以下边界：

1. 是否接受基线缺少可信卡口脚本时拒绝运行，不再默认从当前 `HEAD` bootstrap。
2. 是否允许为隔离测试增加仅具测试所需权限的数据库角色，并将 owner 权限限制在固定
   驱动入口。
3. CI 与本地环境是否允许使用 cgroup、systemd scope 或等效容器作为执行器和卡口的
   终止边界。
4. `--no-pr` 是否采用 `pushed` 状态，还是统一记为 `blocked`。

[report]
task: 0b19dbf1989db0bac2e64b0aa12dfabab5fdc17f
path: escalated
gates: not-run
escalation: docs/plan/proposals/0b19dbf1989db0bac2e64b0aa12dfabab5fdc17f.md
