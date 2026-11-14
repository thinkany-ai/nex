# nex Go SDK

nex 中继的 Go SDK。协议见仓库里的 [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md)，行为和 Rust SDK（`crates/nex`）保持一致。

典型用法：

- **Go 守护进程作为 host**：把本机的 agent API（TCP 或 WebSocket）暴露给同一账号下的其他设备，比如 fastclaw、weclaw 的本地服务。
- **client**：列出账号里的设备，打开到某台设备某个服务的流，或者把远端 TCP 服务转发到本地端口。
- **产品后端（Admin）**：用 admin key 把自己产品的用户映射成 nex 账号，为用户的设备签发凭证。

## 安装

```sh
go get github.com/thinkany-ai/nex/sdk/go
```

```go
import nex "github.com/thinkany-ai/nex/sdk/go"
```

要求 Go 1.22+。唯一的依赖是 `github.com/coder/websocket`。

## 注册设备

一台设备注册一次，拿到 `DeviceCredential`。`DeviceToken` 是长期有效的密钥，要像密码一样保存（Keychain、权限 0600 的文件等），不要写进日志。

```go
ctx := context.Background()

// 新建账号，并把本机注册为第一台设备。中继配置了 admin key 时要传，否则传 ""。
cred, err := nex.Bootstrap(ctx, "https://nex.example.com", nex.BootstrapRequest{
	AccountName: "alice",
	DeviceName:  "mbp",
	App:         "fastclaw", // 可选
}, adminKey)

// 或者：用另一台设备生成的配对码加入已有账号（忽略大小写和 "-"）。
cred, err = nex.Claim(ctx, "https://nex.example.com", nex.ClaimRequest{
	Code:       "ABCD-EFGH",
	DeviceName: "vps",
})
```

## Go 守护进程作为 host

```go
client, err := nex.NewClient("https://nex.example.com", cred)
if err != nil { ... }

host := &nex.Host{
	Client: client,
	Services: []nex.Service{
		nex.WSService("agl", "ws://127.0.0.1:8790/ws"), // 本地 WebSocket 服务
		nex.TCPService("ssh", "127.0.0.1:22"),          // 本地 TCP 服务
		nex.EchoService(),                              // 内置 echo，供 `nex ping` 测试
	},
	Logger:   slog.Default(),
	OnOnline: func(deviceID string) { log.Println("online as", deviceID) },
}

// Run 会一直保持在线，断线自动重连（1s 起步翻倍，最长 30s）。
// ctx 取消时返回 nil；只有遇到致命错误才返回非 nil。
err = host.Run(ctx)
switch {
case err == nil:
	// 正常退出
case errors.Is(err, nex.ErrRevoked), errors.Is(err, nex.ErrUnauthorized):
	// 设备被吊销 / 凭证无效：清掉凭证，提示用户重新配对或登录
case errors.Is(err, nex.ErrReplaced):
	// 同一个凭证在别的进程上线了。不要重启重连，否则两边会互相顶掉。
case errors.Is(err, nex.ErrBadRequest):
	// 服务列表不合法，是代码问题
}
```

注意：

- 收到每个流请求时，host 会先连本地服务；连不上就拒绝（对端收到关闭码 4502，reason 里写着原因），连上了再接受。
- 服务名只能是 1 到 64 个 `A-Z a-z 0-9 . _ -` 字符，不合法时 `Run` 直接返回 `ErrBadRequest`。
- 运行中可以调用 `host.UpdateServices(...)` 更新服务列表，会立刻在当前连接上重新发送 hello。
- **一个凭证同一时间只能有一个 host 连接**。同一台机器上的多个产品要么各自注册设备，要么由一个进程统一暴露所有服务。

### 不占本地端口：HandlerService

服务可以直接在进程内处理，不需要先起一个本地服务器：

```go
nex.HandlerService("agent", nex.KindWS, func(ctx context.Context, conn *websocket.Conn, s nex.StreamInfo) {
	for {
		typ, msg, err := conn.Read(ctx)
		if err != nil {
			return
		}
		reply := handle(s.FromDeviceID, msg)
		if err := conn.Write(ctx, typ, reply); err != nil {
			return
		}
	}
})
```

handler 返回后 SDK 会关闭这个流。`KindTCP` 的 handler 用二进制帧收发原始字节。

## client：访问其他设备

```go
devices, _ := client.Devices(ctx)          // 账号里的设备、是否在线、暴露了哪些服务
conn, err := client.Open(ctx, "mbp", "agl") // 设备可以是 id，也可以是账号内的设备名
if err != nil {
	var apiErr *nex.APIError
	if errors.As(err, &apiErr) && apiErr.Status == 404 {
		// 设备离线或者没有这个服务
	}
	return err
}
defer conn.CloseNow()

conn.Write(ctx, websocket.MessageText, []byte(`{"op":"hello"}`))
_, reply, err := conn.Read(ctx)
switch websocket.CloseStatus(err) {
case nex.CloseRejected: // host 拒绝（比如本地服务没启动）
case nex.CloseOffline:  // 设备刚好掉线
case nex.CloseBusy:     // 并发流太多，稍后重试
case nex.CloseTimeout:  // host 没有及时接受
}
```

数据格式：

- `ws` 服务：帧原样转发，文本还是文本，二进制还是二进制。
- `tcp` 服务：二进制帧承载原始字节，帧边界没有意义；任何一方关闭就是整个连接结束。
- 单条消息最大 16 MiB。SDK 打开的每个连接都已经把读取上限调到 16 MiB（`coder/websocket` 默认只有 32 KiB）。

把远端 TCP 服务映射到本地端口：

```go
ln, _ := net.Listen("tcp", "127.0.0.1:2222")
err := nex.ForwardTCP(ctx, client, "vps", "ssh", ln) // ctx 取消时关闭 ln 并返回 nil
```

其他接口：`WhoAmI`、`CreatePairingCode`（给新设备的配对码）、`Rename`、`Revoke`（可以吊销自己，相当于退出登录）、`Ticket` + `WSURL`（自己拿票据拼 URL，比如交给浏览器打开）。

凭证失效时 REST 调用返回状态码 401 的 `*nex.APIError`，它也匹配 `errors.Is(err, nex.ErrUnauthorized)`。

## 产品后端：Admin

产品后端用 admin key 把自己的用户映射成 nex 账号：同一个 `external_account_id` 永远对应同一个 nex 账号，所以同一个用户在不同产品（fastclaw、weclaw……）里注册的设备可以互相访问。admin key 只能放在服务端。

```go
admin, _ := nex.NewAdmin("https://nex.example.com", os.Getenv("NEX_ADMIN_KEY"))

// 为用户签发一台设备的凭证（账号不存在时自动创建），把 cred 下发给那台设备。
cred, err := admin.IssueDevice(ctx, nex.IssueDeviceRequest{
	ExternalAccountID: "user_42",
	AccountName:       "Alice", // 可选，建账号时用
	DeviceName:        "alice-mbp",
	App:               "fastclaw",
})

devices, err := admin.Devices(ctx, "user_42")       // 账号不存在时 404
err = admin.Revoke(ctx, "user_42", cred.DeviceID)   // 立即吊销
```

admin key 不对返回 403；中继没配置 admin key 时 `/v1/admin/*` 全部返回 403。

## 测试

测试会启动真实的中继（每个测试一个，监听随机端口，内存数据库），并和 Rust CLI 互通：

```sh
cargo build --release          # 在仓库根目录，生成 target/release/nex-server 和 nex
cd sdk/go
go vet ./...
go test -race -count=3 ./...
```

二进制默认从 `../../target/release/` 找，也可以用环境变量 `NEX_SERVER_BIN`、`NEX_BIN` 指定；找不到时相关测试会跳过。
