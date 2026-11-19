# nex

nex 是给 agent 产品用的组网服务：让一个账号下的多台设备（电脑、服务器、手机、浏览器）在任何网络条件下互相访问对方暴露的服务。
它对应 T3 Code 里的 T3 Connect，供 dscode、termany、agl 等需要“连回用户自己机器”的产品复用。

```
 手机 / 浏览器 / 另一台电脑                 nex relay                     用户的电脑（nex up / 嵌入 SDK）
        │  POST /v1/tickets  ───────────▶   │                                   │
        │  WS /v1/connect?ticket ───────▶   │ ── Open{stream} (控制通道) ─────▶ │
        │                                   │ ◀── WS /v1/accept?stream ──────── │ ──▶ 本地服务（agl / shell / …）
        │ ◀═════════════ 字节流或 WebSocket 消息双向转发 ═════════════════════▶ │
```

- **应用（多租户）**：每个需要组网的 agent 产品在控制台 `/admin` 创建一个应用，拿到应用密钥（`nexk_…`）。应用之间的账号和设备完全隔离。
- **账号**：应用的一个终端用户。产品后端用自己的用户 ID 映射成 nex 账号。
- **设备**：账号下的每台设备都有自己的凭证。一台设备可以同时是被访问端（host）和访问端（client）。
- **配对**：已登录设备生成一次性配对码 `XXXX-XXXX`（10 分钟有效），新设备用它加入账号。
- **服务**：host 暴露的本地服务，两种类型：
  - `tcp`：原始字节流，适合 ssh、数据库、任意 TCP 协议。
  - `ws`：WebSocket 消息透传，适合 agl 这类 WebSocket 协议。浏览器可以直接用。
- **票据（ticket）**：每打开一个流前，先用设备凭证换一张一次性票据（60 秒有效）。打开 WebSocket 时只需要把票据放在 URL 里，所以浏览器和手机都不需要自定义请求头。
- **吊销**：吊销后立刻拒绝新的鉴权，已有的控制连接和流也会被断开。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/nex-proto` | 线上协议类型（JSON） |
| `crates/nex-server` | 中继服务：多租户（Postgres）、控制台 `/admin`、配对、控制通道、流转发。可执行文件 `nex-server` |
| `crates/nex` | Rust SDK（`NexClient`、`Host`、`NexAdmin`、`forward_tcp`）和命令行工具 `nex` |
| `sdk/typescript` | TypeScript SDK：浏览器和 Node 通用的 client，Node 专用的 host 和 admin |
| `sdk/go` | Go SDK：client、host、admin |
| `docs/INTEGRATION.md` | **产品接入指南**：账号怎么来、设备怎么划分、各语言的 host 和 client 写法、上线检查清单、排错 |
| `docs/PROTOCOL.md` | 协议规范，各语言 SDK 都按它实现 |
| `docs/DEPLOY.md` | 部署中继：systemd + Caddy，或者 Docker Compose |
| `deploy/`、`Dockerfile` | 部署用的配置文件 |

## 安装

```bash
git clone https://github.com/thinkany-ai/nex.git
cd nex

cargo build --release                 # 产出 target/release/nex-server 和 target/release/nex
cargo install --path crates/nex       # 可选：把 nex 命令行装到 ~/.cargo/bin
```

需要 stable Rust。SDK 另需 Node 22+ 或 Go 1.22+。

各语言 SDK：

```bash
# Rust：在 Cargo.toml 里
# nex = { git = "https://github.com/thinkany-ai/nex.git" }

# Go
go get github.com/thinkany-ai/nex/sdk/go

# TypeScript：暂未发布到 npm，在本仓库 sdk/typescript 下构建后引用
(cd sdk/typescript && npm install && npm run build)
```

中继需要 Postgres 15+。也可以用 Docker 部署（中继 + Postgres + Caddy 自动 HTTPS）：在 `deploy/` 下的 `.env` 里设置 `NEX_DOMAIN`，然后 `cd deploy && docker compose up -d --build`，也支持 Kubernetes 多副本，详见 `docs/DEPLOY.md`。

## 快速开始

```bash

# 1. 启动中继（生产环境放在 TLS 反向代理后面，对外提供 https/wss）
createdb nex
export NEX_DATABASE_URL=postgres://localhost:5432/nex
echo 'a-long-password' | ./target/release/nex-server user create you@example.com   # 第一个管理员
./target/release/nex-server --listen 127.0.0.1:8080
# 打开 http://127.0.0.1:8080/admin 登录，创建应用，拿到应用密钥 nexk_…
# （也可以用命令行：nex-server app create --owner you@example.com --name demo）

# 2. 电脑 A：创建账号，并暴露两个服务
nex init --server http://127.0.0.1:8080 --account demo --name laptop --admin-key nexk_…
nex up --expose agl=ws://127.0.0.1:8790/ws --expose ssh=tcp://127.0.0.1:22

# 3. 电脑 A 上另开一个终端，生成配对码
nex pair                      # 打印出 ABCD-EFGH

# 4. 电脑 B：加入账号后，就可以访问 A 的服务了
nex join ABCD-EFGH --server http://127.0.0.1:8080 --name vps
nex devices
nex forward laptop/ssh --listen 127.0.0.1:2222   # 然后执行 ssh -p 2222 localhost
nex ticket laptop/agl                            # 打印一次性 ws:// 地址，可以直接用 websocat 连接

# B 也可以执行 nex up 暴露自己的服务，A 同样能访问 B，实现多台设备互相访问
```

## 联通测试

```bash
nex up --echo                 # 被测设备：额外暴露内置的 echo 服务
nex ping laptop -c 10         # 另一台设备：经过中继测往返延迟
```

手机或任意浏览器：打开中继地址（比如 `http://192.168.1.68:8787/`）就是测试页面。填入 `nex pair` 生成的配对码加入账号后，可以看到设备列表，并对开了 echo 的设备点 Ping。

注意：在 macOS 上，用 nohup 脱离 ssh 会话运行的 `nex up` 访问不了局域网地址（会报 `No route to host`），这是系统的"本地网络"隐私权限造成的。中继部署在公网时不受影响。

## 开发

```bash
createdb nex_test   # 测试用的库，每个测试在里面建自己的 schema；地址可用 NEX_TEST_DATABASE_URL 覆盖
cargo test     # 包含端到端测试（多租户隔离和控制台流程见 crates/nex/tests/tenancy.rs）：真实启动中继，三台设备互相访问 tcp 和 ws 服务，并覆盖配对、跨账号隔离、吊销
cargo clippy --all-targets

# SDK 测试都会启动真实的中继，并和 Rust CLI 做互通测试，所以要先编译 release
cargo build --release
(cd sdk/typescript && npm install && npm test)
(cd sdk/go && go test -race ./...)
```

## 当前状态

可以用于产品接入，**流量全部经过中继转发**，中继能看到明文。下一步：

1. 端到端加密
2. 直连优先（局域网、Tailscale），中继兜底
3. APNs / FCM 推送
4. 按服务的访问控制（ACL）
5. 多地区部署（控制面全局一套，数据面按地区部署）
6. 接入统一身份服务

详见 `docs/INTEGRATION.md` 末尾的“安全模型与限制”和“路线图”。
