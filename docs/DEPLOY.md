# 部署 nex 中继

第一阶段只需要**一台云服务器**：全球用户共用这一个中继。等带宽、延迟或者国内用户的需求真正出现了，再考虑多地区部署（见本文最后一节）。

## 你需要准备

- **一台 Linux 服务器**：1 核 1 GB 内存就够，主要成本是带宽。面向全球用户的话，东京或香港对国内外都比较折中；用户主要在欧美的话，选美西。
- **一个域名**：比如 `nex.example.com`，A 记录指向这台服务器。
- **一个 admin key**：执行 `openssl rand -hex 32` 生成。它用来创建账号，也是产品后端调用 `/v1/admin/*` 时的凭证，**只能放在服务端**。

## 方式一：systemd + Caddy（推荐）

```bash
# 1. 编译（也可以在本机交叉编译后上传）
cargo build --release -p nex-server
sudo install -m 755 target/release/nex-server /usr/local/bin/

# 2. 用户、目录、密钥
sudo useradd --system --home /var/lib/nex nex
sudo mkdir -p /etc/nex
echo "NEX_ADMIN_KEY=$(openssl rand -hex 32)" | sudo tee /etc/nex/env >/dev/null
sudo chmod 600 /etc/nex/env

# 3. 服务
sudo cp deploy/nex-server.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now nex-server

# 4. TLS：把 deploy/Caddyfile 里的域名改成你自己的
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy

# 5. 检查
curl https://nex.example.com/healthz     # 返回 ok
```

`deploy/Caddyfile` 关掉了代理层的读写超时。如果换成 Nginx，要开启 WebSocket 升级（`proxy_http_version 1.1`、`Upgrade`、`Connection` 这几个头），并把 `proxy_read_timeout` 设成至少 120 秒。中继每 20 秒发一次 ping，所以超时只要大于 20 秒就不会被误断，留足余量更稳。

## 方式二：Docker Compose

```bash
cd deploy
cat > .env <<EOF
NEX_DOMAIN=nex.example.com
NEX_ADMIN_KEY=$(openssl rand -hex 32)
EOF
docker compose up -d --build
```

Compose 会同时启动中继和 Caddy，Caddy 自动申请证书。数据保存在 `nex-data` 这个 volume 里。

## 配置项

| 环境变量 | 命令行参数 | 默认值 | 说明 |
|---|---|---|---|
| `NEX_LISTEN` | `--listen` | `127.0.0.1:8080` | 监听地址。TLS 交给前面的反向代理 |
| `NEX_DB` | `--db` | `nex.db` | SQLite 文件路径。`:memory:` 表示不落盘，只用于测试 |
| `NEX_ADMIN_KEY` | `--admin-key` | 无 | **生产环境必须设置**。不设置时任何人都能创建账号，`/v1/admin/*` 也会被关闭 |
| `NEX_MAX_STREAMS` | `--max-streams` | `128` | 每台设备最多同时参与多少个流 |
| `RUST_LOG` | | `nex_server=info,tower_http=warn` | 日志级别 |

中继的根路径 `/` 是一个测试网页：用配对码加入账号，查看设备，对开了 echo 的设备测延迟。它不包含任何密钥，可以保留。

## 运维

- **健康检查**：`GET /healthz` 返回 `ok`。
- **日志**：设备上线和下线、配对、吊销都会记录在 info 级别。日志里不会出现凭证；票据会出现在请求 URL 里，但它只能用一次，60 秒就过期。
- **备份**：只需要备份 SQLite 文件。运行时要用 `sqlite3 /var/lib/nex/nex.db ".backup /backup/nex-$(date +%F).db"`，不要直接复制文件，因为开了 WAL 模式。
- **升级和重启**：重启会断开所有正在进行的流。host 端会自动重连（退避时间 1 到 30 秒），客户端需要重新打开流。数据库结构在启动时会自动迁移。
- **更换 admin key**：改掉 `/etc/nex/env` 里的值，然后重启中继。已经注册的设备不受影响；只有用到 admin key 的产品后端需要同步更新配置。
- **容量**：在线状态都在内存里，空闲的 host 控制连接几乎不占资源。瓶颈主要是带宽和文件描述符（systemd 配置里已经调高了 `LimitNOFILE`）。

## 什么时候需要多地区部署

现在的中继是单实例的：在线状态、待接入的流和票据都在进程内存里，所以**同一个账号的设备必须连同一个中继**。扩展时按这个顺序考虑：

1. **先纵向扩容**，换一台带宽更大的机器。
2. **按地区拆分**：控制面（账号和设备）全局只有一套，每个地区部署一个数据面中继，host 连到离自己最近的那个。签发票据时告诉客户端去哪个中继连接。这需要改造服务端，详见 `docs/INTEGRATION.md` 的路线图。
3. **国内**：单独部署一套，使用国内服务器和备案域名。
