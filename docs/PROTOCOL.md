# nex 协议规范（v1）

这份文档是 nex 中继对外协议的权威描述，各语言 SDK 都按它实现。Rust 的类型定义在 `crates/nex-proto/src/lib.rs`，两者不一致时以代码为准，并修正本文。

## 1. 约定

- **基础地址**：中继的 base URL，比如 `https://nex.example.com`。可以带路径前缀（`https://example.com/nex/`）。拼接 API 路径时，先保证 base 以 `/` 结尾，再拼接不带开头 `/` 的相对路径。
- **WebSocket 地址**：把 base 的 `http` 换成 `ws`、`https` 换成 `wss`。
- **编码**：所有 REST 请求和响应都是 JSON（UTF-8）。时间都是 Unix 秒（整数）。
- **错误**：非 2xx 响应的 body 是 `{"error": "人能看懂的原因"}`。SDK 应该把 HTTP 状态码和 `error` 一起暴露给调用方。
- **未知字段**：客户端必须忽略响应里不认识的字段，服务端以后会增加字段。

### 鉴权方式

| 方式 | 怎么带 | 用在 |
|---|---|---|
| 设备凭证 | 请求头 `Authorization: Bearer <device_token>` | 绝大部分 REST 接口；`/v1/host` 和 `/v1/accept` 两个 WebSocket |
| 票据 | URL 参数 `?ticket=<ticket>` | 只用于 `/v1/connect`（浏览器开 WebSocket 时不能加请求头） |
| 应用密钥 | 请求头 `x-nex-admin-key: nexk_…`（或 `Authorization: Bearer nexk_…`） | `/v1/admin/*`（产品后端）和 `/v1/bootstrap`。密钥在控制台 `/admin` 创建，决定请求作用于哪个应用 |
| 配对码 | 请求体 | `/v1/devices/claim` |

`device_token` 是长期有效的密钥，要像密码一样保存（Keychain、Keystore、权限为 0600 的文件），不要写进日志。

### 名字规则

- **服务名、app 名**：1 到 64 个字符，只能是 `A-Z a-z 0-9 . _ -`。
- **设备名、账号名**：去掉首尾空白后 1 到 100 个字符，不能含控制字符。
- **external_account_id**：去掉首尾空白后 1 到 200 字节，不能含控制字符。

## 2. 数据类型

```jsonc
// DeviceCredential：注册设备成功后只返回这一次
{ "account_id": "acc_…", "device_id": "dev_…", "device_token": "nexd_…" }

// ServiceInfo
{ "name": "agl", "kind": "ws" }           // kind: "tcp" | "ws"

// DeviceInfo
{
  "id": "dev_…", "name": "mbp", "app": "termany",   // app 可能为 null
  "online": true,                                    // 当前有 host 控制连接
  "services": [ServiceInfo],                         // 离线时为空数组
  "created_at": 1767000000, "last_seen_at": 1767000500,  // last_seen_at 可能为 null
  "is_self": false                                   // 是不是发请求的这台设备
}

// Ticket
{
  "ticket": "next_…", "expires_at": 1767000060,
  "device_id": "dev_…", "service": ServiceInfo,
  "path": "/v1/connect?ticket=next_…"               // 相对中继根的路径
}
```

## 3. REST 接口

| 方法 | 路径 | 鉴权 | 请求体 | 响应 |
|---|---|---|---|---|
| GET | `/healthz` | 无 | | 文本 `ok` |
| POST | `/v1/bootstrap` | 应用密钥（中继以 `--open-bootstrap` 启动时可以不带，只用于开发） | `{account_name, device_name, app?}` | `DeviceCredential` |
| POST | `/v1/devices/claim` | 配对码 | `{code, device_name, app?}` | `DeviceCredential` |
| GET | `/v1/me` | Bearer | | `{account_id, device_id, device_name, app}` |
| POST | `/v1/pairing-codes` | Bearer | 无 | `{code: "ABCD-EFGH", expires_at}` |
| GET | `/v1/devices` | Bearer | | `{devices: [DeviceInfo]}` |
| PATCH | `/v1/devices/{id}` | Bearer | `{name}` | `{ok: true}` |
| DELETE | `/v1/devices/{id}` | Bearer | | `{ok: true}`，立即吊销 |
| POST | `/v1/tickets` | Bearer | `{device, service}` | `Ticket` |
| POST | `/v1/admin/devices` | 应用密钥 | `{external_account_id, account_name?, device_name, app?}` | `DeviceCredential` |
| GET | `/v1/admin/accounts/{external_account_id}/devices` | 应用密钥 | | `{devices: [DeviceInfo]}`（`is_self` 都是 false） |
| DELETE | `/v1/admin/accounts/{external_account_id}/devices/{id}` | 应用密钥 | | `{ok: true}` |

说明：

- **配对码**：格式 `XXXX-XXXX`，10 分钟有效，只能用一次。认领时忽略大小写和 `-`。
- **吊销和改名**：只能操作同一账号下的设备，否则返回 404。可以吊销自己（相当于退出登录）。
- **`/v1/tickets` 的 `device`**：可以是设备 id，也可以是账号内的设备名。名字重复时返回 409，这时要改用 id。设备离线或者没有暴露这个服务时返回 404。
- **`/v1/admin/devices`**：按 `external_account_id` 查找账号，找不到就创建，然后在这个账号下注册一台新设备。产品后端用它把自己的用户映射成 nex 账号。账号按应用隔离：`external_account_id` 只在本应用内唯一，别的应用用同一个 ID 得到的是另一个账号。想让几个产品的设备互通，就让它们用同一个应用的密钥。路径里的 `external_account_id` 要做 URL 编码。

**状态码**：400 参数错误；401 凭证无效或已吊销（SDK 应该提示用户重新配对或登录）；403 应用密钥缺失、错误、已吊销，或应用已停用；404 不存在或离线；409 设备名有歧义。

## 4. 打开一个流（client 端）

1. `POST /v1/tickets`，得到 `Ticket`。票据 **60 秒内有效，只能用一次**。
2. 用 base 的 WebSocket 形式拼上 `ticket.path`，打开 WebSocket。不需要任何请求头，浏览器可以直接用。
   - 票据无效或过期：握手直接失败，HTTP 401。
3. 握手成功后，中继通知目标设备。目标设备接受后，两端就接通了。如果失败，中继用关闭码说明原因：

| 关闭码 | 含义 | 建议处理 |
|---|---|---|
| 4404 | 目标设备离线 | 显示"设备离线" |
| 4429 | 某一端的并发流太多（默认每台设备 128 个） | 稍后重试 |
| 4502 | host 拒绝，reason 里是原因（比如本地服务没启动） | 把 reason 显示给用户 |
| 4504 | host 没在 15 秒内接受 | 重试 |

**接通之后的数据格式：**

- **`tcp` 服务**：用**二进制帧**收发原始字节，帧的边界没有意义。任何一方关闭 WebSocket，就代表整个 TCP 连接结束，不支持半关闭。
- **`ws` 服务**：每一帧原样转发给对端的本地 WebSocket 服务，文本帧还是文本帧，二进制帧还是二进制帧。
- **ping/pong 只在每一段连接上各自生效，不会转发**。
- **正常结束**：任何一端关闭，中继会给另一端发一个**不带关闭码**的关闭帧，各语言库通常显示为 1005 或"无状态码"。本地服务原来的关闭码和原因不会传过去，所以业务上需要的"结束原因"要放在你自己协议的消息里。
- **单条消息最大 16 MiB**。

## 5. host 控制通道

host 是暴露服务的一端，需要能设置请求头，所以不能是浏览器。

1. **连接**：打开 `GET /v1/host`（WebSocket），带上 `Authorization: Bearer <device_token>`。凭证无效时握手返回 HTTP 401，这是致命错误。
2. **10 秒内**发送第一帧 Hello（JSON 文本帧）：
   ```json
   {"type":"hello","protocol":1,"services":[{"name":"agl","kind":"ws"},{"name":"ssh","kind":"tcp"}]}
   ```
   最多 64 个服务，服务名不能重复。不合法时中继用 **4400** 关闭连接（致命错误）。10 秒内没收到 Hello，中继会直接断开、不带关闭码，按普通断线重连就行。
3. 中继回复 `{"type":"ready","device_id":"dev_…"}`，这时设备就在线了。
4. 之后可能收到的消息：
   - `{"type":"open","stream_id":"str_…","service":"agl","from_device_id":"dev_…"}`：有人要连 `agl`。host 应该：
     1. **先连本地服务**。连不上就发 `{"type":"reject","stream_id":"str_…","reason":"local service unavailable: …"}`，到此结束。reason 会放进 client 收到的关闭帧里，WebSocket 关闭帧的原因最多 123 字节，中继会按 UTF-8 字符边界截断，所以最重要的信息要写在前面。
     2. 连上了，就在 **15 秒内**打开 `GET /v1/accept?stream_id=str_…`（WebSocket，带同样的 Bearer 请求头），然后把这个 WebSocket 和本地连接双向对接起来（数据格式见第 4 节）。
     3. 每个 open 都要单独并发处理，不要阻塞控制通道。
   - `{"type":"error","message":"…"}`：比如重新发送的 Hello 不合法。记日志就行。
   - 不认识的 `type`：忽略。
5. **更新服务列表**：随时可以再发一次 Hello。
6. **保活**：
   - 中继每 20 秒发一次 WebSocket ping，host **必须回 pong**（标准库一般会自动回）。连续 3 个周期（60 秒）收不到 host 的任何帧，中继就认为它离线了，断开连接。之后打开流的 client 会马上收到 4404，不用等 15 秒超时。
   - host 这边也要检测连接是否已经静默断开（休眠唤醒、网络切换）：Rust 和 TypeScript SDK 是 65 秒内没收到任何帧就重连；Go SDK 每 30 秒主动 ping 中继一次，35 秒内没收到 pong 就重连。
7. **关闭码：**

| 关闭码 | 含义 | host 应该 |
|---|---|---|
| 4401 | 设备被吊销 | 停止，清掉凭证，提示用户重新配对 |
| 4400 | Hello 不合法 | 停止，这是代码问题 |
| 4409 | **同一个设备凭证在另一个连接上线了，这个连接被顶掉** | 停止，**不要重连**，否则两边会互相顶掉 |
| 其他 / 网络断开 | 中继重启、网络切换等 | 重连 |

**重连策略**（各 SDK 统一）：退避时间从 1 秒开始，每次翻倍，最长 30 秒；如果上一次会话稳定保持了 30 秒以上，退避时间重置为 1 秒。遇到 HTTP 401、4401、4400、4409 时停止重连，把错误交给上层。

**一个凭证同一时间只能有一个 host 连接。** 同一台机器上的多个产品，或者同一个产品的多个进程，如果都要上线，要么每个都注册自己的设备，要么由一个进程统一暴露所有服务。

## 6. 内置 echo 服务（约定，非必须）

测试工具约定使用一个名为 `echo`、类型为 `ws` 的服务：host 收到什么消息就原样发回去。Rust 的 `nex up --echo`、`nex ping` 和中继自带的测试网页都依赖这个约定。各 SDK 最好提供同样的内置实现。

## 7. 直连（可选扩展）

两台设备能直接互通时（同一局域网、同一个 Tailscale tailnet），client 可以绕过中继直接连 host。中继本身不需要任何改动：直连的地址和密钥通过一条普通的中继流交换，所以鉴权仍然由中继负责。目前由 TypeScript SDK 实现（`NexHost` 的 `direct` 选项、`forwardTcp` / `DirectPaths`）。

**发现**：提供直连的 host 在 Hello 里多暴露一个 `ws` 服务 `nex.direct`（保留名）。client 像打开其他服务一样打开它，host 回复一条 JSON 文本消息后关闭：

```json
{"v":1,"key":"<base64, 32 字节>","endpoints":[{"host":"100.64.1.2","port":51995,"kind":"tailscale"},{"host":"192.168.1.45","port":51994,"kind":"lan"}],"services":["ssh"]}
```

- `key` 是只给这台 client 的 PSK：`HMAC-SHA256(host 进程内随机主密钥, "nex-direct/v1/" + from_device_id)`。host 重启后主密钥换新，旧 key 全部失效。
- `endpoints` 只包含私有地址：`10/8`、`172.16/12`、`192.168/16`（`lan`），`100.64/10`、`fd7a:115c:a1e0::/48`（`tailscale`），或配置里显式指定的地址（`custom`）。
- `services` 是能直连的服务：只有本地目标是 TCP 端口的 `tcp` 服务。

**连接**：

1. client 用 TLS 1.2 连到某个 endpoint，密码套件只用 ECDHE-PSK（如 `ECDHE-PSK-CHACHA20-POLY1305`），PSK identity 是 `nex1:<client 的 device_id>`，PSK 是上面的 `key`。没有证书。
2. host 只接受通过中继拿过 key 的 device_id。host 每 2 分钟用 `GET /v1/devices` 复核一次，不在列表里的（被吊销）立即移除；中继连不上时保留现有授权，等中继恢复后再复核。client 在中继连不上时也继续用上次拿到的 key 和地址。
3. 握手成功后 client 发一行 JSON：`{"service":"ssh"}\n`（探测用 `{"probe":true}\n`）。
4. host 回一行：`{"ok":true}\n` 之后两边就是原始 TCP 字节；或者 `{"ok":false,"error":"…"}\n` 后关闭。

**选路**（SDK 行为，不属于线上协议）：client 每 15 秒探测每个 endpoint 的建连耗时，把它和"打开一条中继流"的耗时比较；直连不比中继慢一倍以上就走直连，否则走中继。直连失败时这条连接立即改走中继；所有 endpoint 都连不上、或者 PSK 被拒时，重新通过 `nex.direct` 拿地址和密钥。

### 7.1 配对设备（密钥不经过中继）

上面的直连密钥由中继发放，所以中继运营方理论上可以冒充设备。配对模式下密钥由用户自己在两台设备之间交换，中继拿不到：

- 每台设备在本地生成 X25519 身份密钥对，私钥不离开设备。公钥写成 `nexpk1.<base64url 32 字节>`，可以公开展示；指纹是公钥 SHA-256 的前 20 个字符（Base32 去掉易混字符），分 5 组，供两边肉眼核对。
- 用户把对方的公钥填到本机（复制粘贴、扫码、核对指纹都行）。两边各自算出同一把 PSK：`HKDF-SHA256(X25519(自己私钥, 对方公钥), salt="nex-paired/v1", info=两把公钥按字节序拼接)`。
- TLS 握手同第 7 节，只是 PSK identity 换成 `nex2:<自己公钥的 base64url>`，host 只接受已配对公钥列表里的 identity。

配对后的连接有三种走法，握手和请求行完全一样：

| 走法 | 说明 |
|---|---|
| 直连 | 同第 7 节，换用配对密钥 |
| 经中继，端到端加密 | 打开内置 `tcp` 服务 `nex.e2e`，在这条中继流上跑同样的 TLS-PSK 握手和请求行。中继只看到密文 |
| 完全不用中继 | host 在固定端口（默认 47428）监听，client 直接填对方的局域网 IP / Tailscale IP / MagicDNS 名字。不需要 nex 账号 |

规则：

- client 配了对方公钥后，**每个连接都必须用配对密钥**，对方不接受就失败，绝不悄悄退回明文。
- host 可以选 `strict`：tcp 服务只接受配对设备，普通中继流（4502 拒绝）和中继发的直连密钥（`nex.direct` 返回空 `key`）都不再接受。
- 被吊销的设备：配对模式下由用户在本机删掉对方公钥，不依赖中继。

## 8. 兼容性

- 增加字段、增加接口、增加关闭码，都不算破坏性变更，`protocol` 保持为 1。
- 破坏性变更会把 `PROTOCOL_VERSION` 加 1，中继会在一段时间内同时支持新旧两个版本。
