# OCI 部署的 KMS 凭据接入与验收

本文是**OCI VPS 部署形态下 KMS 凭据来源、IAM 配置模板与生产验收清单的唯一定义处**。
变量清单与预检规则见 `../eng/01-配置与Secret.md` §二；方案取舍与本地探针证据见
`../../plan/evidence/PRE-003-KmsKek验证-2026-10-16.md`。本文不重复这两处。

OCI 主机不是 AWS 计算资源，没有 ECS task role、EKS IRSA 或 EC2 instance role。定案：

```text
过渡（G）  IAM user 静态 key，经环境变量注入；权限只到单把 key，按出口 IP 收紧
目标（A）  IAM Roles Anywhere：X.509 证书经 aws_signing_helper credential-process
           换取短期会话凭据，SDK 到期前自动重取，进程不重启
排除（B）  helper 定时刷新 shared credentials 文件 —— SDK 只解析一次文件，已反证
```

`control-plane`、`bot-gateway`、`delivery`、`mtproto-worker` 四个角色持有 AWS 凭据，其余角色配了
就会被预检拒绝。四个角色分两套 IAM 身份（ADR-0032）：`control-plane` 只能 `kms:Encrypt`，
另外三个能 `kms:Encrypt` 与 `kms:Decrypt`。两套身份不得共用一份凭据。

---

## 一、两阶段共用的前提

- **生产 key 区域：`ap-southeast-2`**（2026-10-17 定）。生产 key 必须是另建的一把，
  不得沿用 PRE-003 真实往返用的测试 key，即使两者同区域。
  **与辖区的张力未解决**：辖区已定为欧盟（`../decisions/ADR-0028-司法辖区定为欧盟.md`），
  那份 ADR 把部署地域列为合规变量。KMS 里只放 KEK，不放租户数据，但 KEK 所在区域
  仍可能被欧盟客户问到。这一项要由 ADR 正式承接（新写一份，或在 ADR-0028 的后续节里
  标注），本文只记录区域取值。
- **固定出口 IP。** OCI 实例绑定 reserved public IP（或经 NAT gateway 固定出口），
  G 阶段的 `aws:SourceIp` 条件依赖它。出口 IP 一变，KMS 调用就全部 AccessDenied。
- **禁用 IMDS。** `TGM_KEK_BACKEND=kms` 时预检要求 `AWS_EC2_METADATA_DISABLED=true`：
  OCI 自己的元数据服务也在 `169.254.169.254`，凭据链末端不能去碰它。
- **服务账户 HOME 下不放 `.aws/`。** 预检只看变量名，看不到默认路径的 profile 文件。
- **key policy 放行 IAM。** 默认 key policy 允许账户内 IAM 策略授权；
  如果改成了自定义 key policy，需要显式列出下面的 principal。

## 二、G 阶段：IAM user 静态 key

### 2.1 IAM 模板

两个 IAM user，都不开控制台登录，各挂一条内联策略：

```text
tgm-kms-<env>-encrypt   control-plane 专用，只有 kms:Encrypt
tgm-kms-<env>           bot-gateway / delivery / mtproto-worker，kms:Encrypt + kms:Decrypt
```

`tgm-kms-<env>-encrypt`：

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "TgmKekWrapOnly",
      "Effect": "Allow",
      "Action": ["kms:Encrypt"],
      "Resource": "arn:aws:kms:ap-southeast-2:<account-id>:key/<key-id>",
      "Condition": {
        "IpAddress": { "aws:SourceIp": "<oci-egress-ip>/32" },
        "ForAllValues:StringEquals": {
          "kms:EncryptionContextKeys": ["tgm:tenant_id", "tgm:identity_id"]
        }
      }
    }
  ]
}
```

`tgm-kms-<env>`：

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "TgmKekWrapUnwrap",
      "Effect": "Allow",
      "Action": ["kms:Encrypt", "kms:Decrypt"],
      "Resource": "arn:aws:kms:ap-southeast-2:<account-id>:key/<key-id>",
      "Condition": {
        "IpAddress": { "aws:SourceIp": "<oci-egress-ip>/32" },
        "ForAllValues:StringEquals": {
          "kms:EncryptionContextKeys": ["tgm:tenant_id", "tgm:identity_id"]
        }
      }
    }
  ]
}
```

`kms:EncryptionContextKeys` 条件要求请求只带这两个上下文键（`ForAllValues` 对空集合也成立，
所以它限制的是「不许多带别的键」，不是「必须带」；必须带由代码保证，`KmsKek` 每次都发）。

`Encrypt`/`Decrypt` 正好是 `KmsKek` 用到的两个 API（`bins/tgm/src/kms_kek.rs`），
不给 `GenerateDataKey`、`DescribeKey` 等。

### 2.2 环境模板

每个 KMS 角色一份，文件权限 `0600`，属主是运行用户。control-plane 那份填
`tgm-kms-<env>-encrypt` 的 key，另外三份填 `tgm-kms-<env>` 的 key：

```bash
TGM_KEK_BACKEND=kms
TGM_KMS_KEY_ARN=arn:aws:kms:ap-southeast-2:<account-id>:key/<key-id>
AWS_EC2_METADATA_DISABLED=true
AWS_ACCESS_KEY_ID=<access-key-id>
AWS_SECRET_ACCESS_KEY=<secret-access-key>
```

G 阶段不得同时设置 `AWS_PROFILE`、`AWS_CONFIG_FILE`、`AWS_SHARED_CREDENTIALS_FILE` 等
其他凭据来源，预检会拒绝。

### 2.3 轮换

IAM user 最多有两把 access key，轮换按以下顺序做，不中断服务：

```text
1 创建第二把 key
2 更新三份环境文件，逐个重启 KMS 角色，确认启动日志出现
  「KEK 往返自测通过：backend=kms」
3 停用（Inactive）旧 key，观察一个完整业务周期
4 删除旧 key
```

轮换周期属于运维决定，本文不定。G 阶段持续越久，长期 key 的暴露窗口越大，
所以本阶段应设退出期限。

## 三、A 阶段：IAM Roles Anywhere

### 3.1 AWS 侧

```text
1 Trust anchor：选外部 CA（上传 CA 证书 bundle）或 AWS Private CA。CA 私钥
  不得放在 OCI 主机上
2 IAM role：两个，权限策略分别同 §2.1 的两份（可保留 aws:SourceIp），信任策略见下。
  control-plane 用只含 kms:Encrypt 的那个
3 Profile：每个 role 一个，会话时长按默认 3600 秒起步
4 签发终端证书：每台主机每套身份一张，CN 写主机标识加用途（如 <host>-encrypt），
  私钥在主机上生成，不出机。两张证书 CN 不同，信任策略各按自己的 CN 收紧
```

role 信任策略（只信任本 trust anchor，并按证书 CN 收紧）：

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "Service": "rolesanywhere.amazonaws.com" },
      "Action": ["sts:AssumeRole", "sts:TagSession", "sts:SetSourceIdentity"],
      "Condition": {
        "ArnEquals": {
          "aws:SourceArn": "arn:aws:rolesanywhere:ap-southeast-2:<account-id>:trust-anchor/<trust-anchor-id>"
        },
        "StringEquals": {
          "aws:PrincipalTag/x509Subject/CN": "<host-cn>"
        }
      }
    }
  ]
}
```

### 3.2 主机侧

`/etc/tgm/aws/config`，属主 `root:<运行组>`，权限 `0640`：

```ini
[default]
credential_process = /usr/local/bin/aws_signing_helper credential-process --certificate /etc/tgm/pki/host.crt --private-key /etc/tgm/pki/host.key --trust-anchor-arn <trust-anchor-arn> --profile-arn <profile-arn> --role-arn <role-arn>
```

SDK 用 `sh -c` 执行这一行，路径里不要带需要转义的字符。私钥 `host.key` 只给运行用户读，
权限 `0400`。

环境模板（替换 §2.2 的最后两行）：

```bash
TGM_KEK_BACKEND=kms
TGM_KMS_KEY_ARN=arn:aws:kms:ap-southeast-2:<account-id>:key/<key-id>
AWS_EC2_METADATA_DISABLED=true
AWS_CONFIG_FILE=/etc/tgm/aws/config
AWS_SHARED_CREDENTIALS_FILE=/etc/tgm/aws/empty
```

`AWS_SHARED_CREDENTIALS_FILE` 指向一个空文件，防止 SDK 回落去读 HOME 下的默认 credentials。

## 四、验收清单

每项都要留证据（命令输出或 CloudTrail 事件截取），记录到 `docs/plan/evidence/`，
**不得贴出 secret、会话 token 或证书私钥**。

### 4.1 G 阶段

```text
[ ] 三个解密角色启动日志出现「KEK 往返自测通过」
[ ] control-plane 启动日志出现「KEK encrypt-only 自测通过：backend=kms（Decrypt 已被拒）」
[ ] 反证：给 control-plane 换上解密角色的凭据，它拒绝启动（encrypt-only 自测报 DecryptAllowed）
[ ] CloudTrail 的 Encrypt/Decrypt 事件 requestParameters.encryptionContext 带 tgm:tenant_id 与 tgm:identity_id
[ ] 反证：去掉 AWS_EC2_METADATA_DISABLED 后，预检拒绝启动
[ ] 反证：环境里再加 AWS_CONFIG_FILE，预检报静态 key 遮蔽来源
[ ] 反证：同一把 key 从非 OCI 出口 IP 调 kms:Encrypt，返回 AccessDenied
[ ] 反证：同一把 key 调策略外的 API（如 kms:GenerateDataKey），返回 AccessDenied
[ ] CloudTrail 中 Encrypt/Decrypt 的 userIdentity 是 tgm-kms-<env>，sourceIPAddress 是 OCI 出口
[ ] 按 §2.3 完整走一次轮换，旧 key 停用后服务正常
```

### 4.2 A 阶段

```text
[ ] 主机上单独运行 aws_signing_helper credential-process 能返回凭据（只记录成功与否和 Expiration）
[ ] 三个解密角色启动往返自测通过，control-plane encrypt-only 自测通过
[ ] CloudTrail 中 userIdentity 是 assumed-role/<role>，不再是 IAM user
[ ] 反证：换一张不同 CN 的证书，AssumeRole 失败
[ ] 反证：在环境里残留 AWS_ACCESS_KEY_ID/SECRET，预检拒绝启动
[ ] 凭据跨过会话到期仍可用（见下方说明）
[ ] 切换完成后停用并删除 G 阶段 IAM user 的 key
```

**运行期刷新目前只有一个调用方。** `control-plane` 的 Bot Token 录入
（`POST /api/v1/identities/bot`）会在运行期调 `kms:Encrypt`，可以用它补验：进程跑过会话时长后
再提交一次 token，应当成功。三个解密角色在运行期还没有 `Decrypt` 调用方，那一侧要等
session 加密落库等调用方接入后补验。SDK 的自动重取已经在本地探针里验过（见验证记录）。

## 五、本文的局限

- 本文所有模板都没有在真实 AWS 账号和 OCI 主机上跑过，只有凭据链行为经过本地 mock 探针验证。
- CA 选型（外部 CA 还是 AWS Private CA）、轮换周期、G 阶段退出期限都未定；生产 key 区域与欧盟辖区的关系待 ADR 承接（§一）。
- 部署形态（容器还是 systemd）由 `00-部署与运行时.md` 定；本文的环境模板两种形态都适用，
  不规定注入方式。
