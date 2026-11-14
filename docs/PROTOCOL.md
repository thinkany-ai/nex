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
| admin key | 请求头 `x-nex-admin-key: <key>` | `/v1/admin/*`（产品后端）；以及中继配置了 key 时的 `/v1/bootstrap` |
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
| POST | `/v1/bootstrap` | admin key（中继配置了时才需要） | `{account_name, device_name, app?}` | `DeviceCredential` |
| POST | `/v1/devices/claim` | 配对码 | `{code, device_name, app?}` | `DeviceCredential` |
| GET | `/v1/me` | Bearer | | `{account_id, device_id, device_name, app}` |
| POST | `/v1/pairing-codes` | Bearer | 无 | `{code: "ABCD-EFGH", expires_at}` |
| GET | `/v1/devices` | Bearer | | `{devices: [DeviceInfo]}` |
| PATCH | `/v1/devices/{id}` | Bearer | `{name}` | `{ok: true}` |
| DELETE | `/v1/devices/{id}` | Bearer | | `{ok: true}`，立即吊销 |
| POST | `/v1/tickets` | Bearer | `{device, service}` | `Ticket` |
| POST | `/v1/admin/devices` | admin key | `{external_account_id, account_name?, device_name, app?}` | `DeviceCredential` |
| GET | `/v1/admin/accounts/{external_account_id}/devices` | admin key | | `{devices: [DeviceInfo]}`（`is_self` 都是 false） |
| DELETE | `/v1/admin/accounts/{external_account_id}/devices/{id}` | admin key | | `{ok: true}` |

说明：

- **配对码**：格式 `XXXX-XXXX`，10 分钟有效，只能用一次。认领时忽略大小写和 `-`。
- **吊销和改名**：只能操作同一账号下的设备，否则返回 404。可以吊销自己（相当于退出登录）。
- **`/v1/tickets` 的 `device`**：可以是设备 id，也可以是账号内的设备名。名字重复时返回 409，这时要改用 id。设备离线或者没有暴露这个服务时返回 404。
- **`/v1/admin/devices`**：按 `external_account_id` 查找账号，找不到就创建，然后在这个账号下注册一台新设备。产品后端用它把自己的用户映射成 nex 账号。同一个用户在不同产品里拿到的设备都在同一个 nex 账号下，所以可以互相访问。中继没有配置 admin key 时，`/v1/admin/*` 全部返回 403。路径里的 `external_account_id` 要做 URL 编码。

**状态码**：400 参数错误；401 凭证无效或已吊销（SDK 应该提示用户重新配对或登录）；403 admin key 不对；404 不存在或离线；409 设备名有歧义。

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

## 7. 兼容性

- 增加字段、增加接口、增加关闭码，都不算破坏性变更，`protocol` 保持为 1。
- 破坏性变更会把 `PROTOCOL_VERSION` 加 1，中继会在一段时间内同时支持新旧两个版本。
