# 部署 nex 中继

第一阶段只需要**一台云服务器**：全球用户共用这一个中继。等带宽、延迟或者国内用户的需求真正出现了，再考虑多地区部署（见本文最后一节）。

## 你需要准备

- **一台 Linux 服务器**：1 核 1 GB 内存就够，主要成本是带宽。面向全球用户的话，东京或香港对国内外都比较折中；用户主要在欧美的话，选美西。
- **一个 Postgres 15+ 数据库**：建一个专用的库和用户，比如库 `nex`、用户 `nex`（库和 `public` schema 都归它所有）。表结构在中继启动时自动迁移。
- **一个域名**：比如 `nex.example.com`，A 记录指向这台服务器。

## 租户模型

```
控制台用户（/admin 登录） ─┬─ 应用 A（一个 agent 产品）─┬─ 应用密钥 nexk_…（最多 2 个有效）
                          │                            └─ 账号（终端用户）── 设备
                          └─ 应用 B …
```

- 控制台用户分管理员和成员。成员只能看到自己创建的应用；管理员能看到所有应用，管理用户和是否开放注册。
- 产品后端和客户端用应用密钥调用 `/v1/admin/*` 和 `/v1/bootstrap`，作用范围只在这个应用内。
- 停用应用会立即断开它的所有设备，并拒绝它的密钥和设备凭证；删除应用会删除它的全部账号和设备。

## 方式一：systemd + Caddy（推荐）

```bash
# 1. 编译（也可以在本机交叉编译后上传）
cargo build --release -p nex-server
sudo install -m 755 target/release/nex-server /usr/local/bin/

# 2. 用户、配置
sudo useradd --system --home /var/lib/nex nex
sudo mkdir -p /etc/nex
sudo tee /etc/nex/env >/dev/null <<EOF
NEX_DATABASE_URL=postgres://nex:<password>@db.internal:5432/nex
NEX_PUBLIC_URL=https://nex.example.com
EOF
sudo chmod 600 /etc/nex/env

# 3. 第一个管理员（密码从标准输入读）
echo 'a-long-password' | sudo env $(sudo cat /etc/nex/env | xargs) nex-server user create you@example.com

# 4. 服务
sudo cp deploy/nex-server.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now nex-server

# 5. TLS：把 deploy/Caddyfile 里的域名改成你自己的
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy

# 6. 检查
curl https://nex.example.com/healthz     # 返回 ok
# 打开 https://nex.example.com/admin 登录，创建应用
```

`deploy/Caddyfile` 关掉了代理层的读写超时。如果换成 Nginx，要开启 WebSocket 升级（`proxy_http_version 1.1`、`Upgrade`、`Connection` 这几个头），并把 `proxy_read_timeout` 设成至少 120 秒。中继每 20 秒发一次 ping，所以超时只要大于 20 秒就不会被误断，留足余量更稳。

## 方式二：Docker Compose

```bash
cd deploy
cat > .env <<EOF
NEX_DOMAIN=nex.example.com
POSTGRES_PASSWORD=$(openssl rand -hex 16)
EOF
docker compose up -d --build
echo 'a-long-password' | docker compose exec -T nex nex-server user create you@example.com
```

Compose 会同时启动中继、Postgres 和 Caddy，Caddy 自动申请证书。数据保存在 `pg-data` 这个 volume 里。

## 命令行

| 命令 | 作用 |
|---|---|
| `nex-server` | 启动中继 |
| `nex-server user create <email> [--role admin\|member]` | 创建控制台用户，密码从标准输入读，默认管理员 |
| `nex-server user password <email>` | 重设密码（忘记密码时用） |
| `nex-server app create --owner <email> --name <name>` | 创建应用并打印第一个密钥（自动化用） |

## 配置项

| 环境变量 | 命令行参数 | 默认值 | 说明 |
|---|---|---|---|
| `NEX_DATABASE_URL` | `--database-url` | 无，必填 | Postgres 地址，比如 `postgres://nex:…@host:5432/nex?sslmode=disable` |
| `NEX_PUBLIC_URL` | `--public-url` | 无 | 控制台的公开地址（`https://…`）。控制台的写操作必须来自这个来源，https 时会话 cookie 带 Secure。不设置时信任请求自己的 Host，只用于本地 |
| `NEX_LISTEN` | `--listen` | `127.0.0.1:8080` | 监听地址。TLS 交给前面的反向代理 |
| `NEX_MAX_STREAMS` | `--max-streams` | `128` | 每台设备最多同时参与多少个流 |
| `NEX_ADMIN_KEY` | `--admin-key` | 无 | 兼容旧版：设置后作为内置应用 `default` 的一个额外密钥。新接入请用控制台创建的应用密钥 |
| `NEX_OPEN_BOOTSTRAP` | `--open-bootstrap` | 关 | 允许不带密钥调用 `/v1/bootstrap`（账号建在 `default` 应用里）。**只用于开发和测试** |
| `NEX_DATABASE_SCHEMA` | `--database-schema` | 无 | 把表建在这个 schema 里而不是 `public`，测试用 |
| `RUST_LOG` | | `nex_server=info,tower_http=warn` | 日志级别 |

中继的根路径 `/` 是一个测试网页：用配对码加入账号，查看设备，对开了 echo 的设备测延迟。控制台在 `/admin`。

## 运维

- **健康检查**：`GET /healthz` 返回 `ok`。
- **日志**：设备上线和下线、配对、吊销、应用和用户的变更都会记录在 info 级别。日志里不会出现凭证和密钥；票据会出现在请求 URL 里，但它只能用一次，60 秒就过期。
- **备份**：备份 Postgres 即可（`pg_dump` 或云数据库的自动备份）。凭证、密钥、密码都只存哈希。
- **升级和重启**：Pod 停止时，经过该 Pod 的流会断开。host 会自动重连（退避 1 到 30 秒），客户端需要重新打开流。数据库结构在启动时自动迁移（`_sqlx_migrations` 表记录版本）。
- **轮换应用密钥**：在控制台新建第二个密钥，部署到产品后端，再吊销旧的。已注册的设备不受影响。
- **容量**：观察各 Pod 的带宽、活跃连接、文件描述符和 CPU，以及 PostgreSQL 的连接数和签票延迟（每个 Pod 的连接池上限为 10）。活跃 WebSocket 在连接建立时绑定到 Pod，扩容会接收新连接。

## Kubernetes 多副本

- 所有副本连接同一个 PostgreSQL 数据库和 schema，使用相同的鉴权配置。通过 Downward API 把 `status.podIP` 注入 `NEX_PEER_IP`，Pod 之间通过 `NEX_PEER_PORT`（默认 `8081`）通信。对外 Service 和 Ingress 只转发 HTTP 端口 `8080`，并支持 WebSocket 升级。网络策略允许 Nex Pod 互访私有端口。
- PostgreSQL 保存在线设备的归属、一次性票据、Pod 租约和全局设备并发额度。任意 Pod 都能处理 API 请求，并把 WebSocket 转发到 host 所在的 Pod。流量直接走 Pod 间 TCP。保持现有 SDK 和 API 配置即可，无需负载均衡会话粘性。
- 每个 host 的待发控制消息队列最多 64 条，满时新请求收到 `4429`。WebSocket 写入超过 10 秒会结束连接，关闭帧最多等待 2 秒。Pod 间转发在任一端收到 EOF 或写入超过 10 秒后关闭两端。
- Pod 异常退出时，host 可连接其他副本。失效的归属和并发额度最多约 15 秒后过期。吊销请求立即阻止新的鉴权，其他健康副本上的已有连接通常在下一个约 1 秒的同步周期内关闭。
- 第一次从单实例旧版升级时，旧 host 会在 Pod 退出后自动重连。旧镜像可能因 SQLx 发现更新的迁移记录而无法启动。需要临时缩容时，保留新版镜像，把副本数调回 1。

## 多地区部署

同一组副本应部署在网络互通、能低延迟访问同一 PostgreSQL 实例的集群内。将控制面和各地区中继分开属于后续工作。不同地区也可以运行独立的 Nex 部署，各自管理凭证和设备。
