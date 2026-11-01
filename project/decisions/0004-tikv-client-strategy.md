# ADR-0004: TiKV 客户端策略（tikv-client 优先，kvproto 兜底）

- 状态：**accepted**（2026-11-01 书面 spike：先用 tikv-client 0.4.x，缺 Cop/批量锁再兜底 kvproto）
- 日期：2026-10-19
- 关联：requirements C4；architecture §5；risks R3

## 背景

TiDB 通过 Go 的 `client-go` 访问 PD/TiKV，包含：

- PD：TSO、Region 元数据、调度、etcd（Schema 版本/选主）；
- TiKV：KV RPC、PessimisticLock、Prewrite/Commit、Coprocessor DAG、BatchCop/MPP。

Rust 侧可选：

1. `tikv-client`（client-rust，当前 0.4.x）：高层事务/原始 KV、RegionCache、PD TSO；`RawClient::coprocessor` 可发自定义 Cop 请求。
2. `kvproto` + gRPC（tonic/grpcio）：直接使用 protobuf，控制力最强，但需自研 RegionCache/重试/2PC。
3. 其它第三方客户端：成熟度/维护性不确定。

## 决策（建议）

**首选 `tikv-client 0.4.x`，封装在 `pkg/store/driver` 后；若关键能力缺失，再用 `kvproto + tonic` 实现最小客户端兜底。**

关键能力清单（Phase 0 必须逐项验证）：

| 能力 | 用途 | 验证方式 |
|---|---|---|
| `TransactionClient::new`（PD endpoints） | 建立连接 | 连接本地 PD |
| `begin_optimistic` / `begin_pessimistic` | 事务 | 读写/提交 |
| `get`/`put`/`delete`/`scan`/`batch_get` | KV | 基本读写 |
| `get_for_update` | 悲观锁 / `FOR UPDATE` | 两事务冲突 |
| `commit` / `rollback` | 2PC | 提交后可见/回滚不可见 |
| `current_timestamp` / PD TSO | 时间戳 | 单调递增 |
| RegionCache | 路由 | 写入后 LocateKey |
| `RawClient::coprocessor` | DAG 下推 | 发一个 Selection DAG |
| TLS / API v2 / Keyspace | 生产连接 | 配置项/握手 |
| 批量锁/`lock_keys` | DML 加锁 | 若缺失，记录降级方案 |
| 错误类型映射 | 冲突/Region 错误 | 映射到 `pkg/kv` 错误 |

## 后果

### 正面

- 复用成熟客户端，快速打通事务/路由/TSO。
- Rust 生态内维护，避免 FFI/Go 运行时。
- `pkg/store/driver` 隔离后，未来可替换。

### 负面 / 代价

- `tikv-client` 可能缺少 Coprocessor 高层 API、PessimisticLock 批量、Async Commit/1PC、Keyspace 等；需要适配或兜底。
- 其错误类型/重试语义与 client-go 不完全一致，需要映射。
- 版本升级可能引入 API 变化。

## 备选方案

| 方案 | 结论 |
|---|---|
| 纯 `kvproto` + tonic | 控制力强但工程量大；作为兜底 |
| FFI 调 Go client-go | 引入 Go 运行时/构建复杂度，拒绝 |
| 自己实现 2PC/RegionCache | 重复造轮子，风险高；仅兜底 |

## Phase 0 Spike 交付

- [x] `tools/spikes/tikv-client/` 乐观事务最小示例（TSO + begin/put/get/commit）。
- [x] 能力矩阵（API 审阅，待实集群勾选）。
- [x] 失败/缺失项兜底：Cop DAG、批量 pessimistic lock、Keyspace/TLS → `kvproto + tonic` 最小客户端，藏在 `pkg/store/driver`。
- [x] 结论：继续首选 `tikv-client 0.4.x`；Planner/Executor 禁止直接依赖客户端类型。

### 能力矩阵（2026-11-01）

| 能力 | 结论 |
|---|---|
| `TransactionClient::new` / `current_timestamp` | API 存在；spike 代码已写 |
| `begin_optimistic` + get/put/commit | API 存在；spike 代码已写 |
| `begin_pessimistic` / `get_for_update` | crate 暴露，待实集群 |
| `RawClient::coprocessor` | 有原始入口，无 DAG 高层封装 → 自建 |
| 批量锁 / lock_keys | 可能缺失 → kvproto 兜底 |
| TLS / API v2 / Keyspace | 配置面待验证 |

实集群验证仍缺：本机未默认运行 v8.5.8 PD/TiKV。解除条件：`make docker-up` 后跑 `tools/spikes/tikv-client`。

## 验证方式

- 本地 PD/TiKV（优先 8.5.x）跑通 demo。
- 失败项在 `progress.md` 记录，并在 ADR 更新。
- Phase 5/6 的实际事务/Cop 测试作为最终验证。

## 参考

- `tikv-client` 0.4.0 docs（TransactionClient/RawClient/RegionCache）
- `pkg/store/driver/tikv_driver.go`
- `pkg/kv/kv.go`
