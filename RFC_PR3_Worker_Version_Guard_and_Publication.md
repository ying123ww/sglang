# RFC / PR 3：Worker 接单版本保护、排空更新与 strict 激活

**建议 PR 标题：** `feat(srt): guard PD execution epochs and publish weights after drain`

**状态：** Draft。本文是实现与评审规格，不是已实现功能或实验报告。  
**基线：** 本次上传快照 `3ed56a341b57eb57c8e37e0cec353e67ebb35c3b`，叠加 PR 1、PR 2。  
**前置依赖：** Gateway 的执行元数据、共享 pair planner、请求绑定、约束/回执消费及有限重试。  
**交付目标：** 在明确支持的 Worker 配置和传输上，闭合选路之后发生权重更新的竞态，激活执行级 strict。

## 1. 摘要

Gateway 比较版本只能约束一次观察，不能阻止 Worker 在选路之后、接单之前切换权重。即使两端在接单时版本相同，如果更新能够穿过等待队列、KV 传输或未完成的 decode，仍然不能保证一次请求属于一个发布版本。

本 PR 增加 Worker 本地的执行 epoch 保护。请求携带 expected key，在原子接单边界内核对当前发布状态；保护覆盖从接单预留、进入调度器到计算和传输资源安全退休的全过程。权重更新先关闭新接单、排空旧工作，再独占加载和清理缓存，最后发布新的 Ready 状态。

复用现有 `model_update_lock`、请求终止路径及 `Scheduler.is_fully_idle()`，不建立全局锁服务、P/D 两阶段提交或双版本权重驻留。新增的是可验证的接单和退休契约，而不是再增加一个可随意改写的版本标签。

## 2. 已有机制与具体缺口

### 2.1 已有本地互斥

`TokenizerManager.generate_request` 已在普通生成路径中持有 `model_update_lock.reader_lock`。`update_weights_from_disk` 在未暂停时使用 writer lock，但暂停时采用 `nullcontext()`；其 TokenizerManager 版本更新还在该锁上下文之后执行。[^S1][^S2]

因此不能把问题描述为“当前引擎没有更新锁”。缺口是期望版本校验、请求实际生命周期与发布状态之间没有本系列需要的统一契约。

### 2.2 已有 idle 和传输结束路径

当前 `Scheduler.is_fully_idle()` 已检查运行 batch、chunked request、overlap result queue、grammar queue，以及 P/D 的 bootstrap、preallocation、transfer、retracted 等队列；它不是只检查 `running_batch`。[^S3]

Prefill 的输出与 KV transfer 完成有专门路径，涉及 sender clear、tree finish、metadata buffer 释放。新的退休确认应接在这些实际清理之后，而不是在 HTTP handler 退出时直接释放。[^S4]

### 2.3 版本标签、暂停和内部重建

`/update_weight_version` 可独立改变标签，不等于加载权重。`pause_generation` 的 in-place/retract 语义本来允许不同的执行恢复方式，不能直接作为 strict 的 drain。[^S5][^S6]

`Req.build_rebootstrap_payload()` 和 `submit_prefill_recompute()` 可以直接向原 P 的 HTTP `/generate` 发请求。这条调用不一定经过 Gateway，必须同时受 expected key 约束。[^S7][^S8]

## 3. 首版支持范围

首版使用固定 revision 的 Dense text Qwen3 小模型、无 LoRA、TP=DP=PP=1、一个接单所有者、固定 Tokenizer/模板、Mooncake P/D。验证流式及非流式、单样本 `n=1`；不要求禁用正常的 chunked prefill，但其实际生命周期必须被覆盖。

支持进程不可变的滚动替换，以及 **drain 后通过 `update_weights_from_disk` 更新同构的完整 checkpoint**。不支持让同一个 strict 请求跨原地权重更新继续生成。

HTTP 覆盖 `/generate`、文本 `/v1/chat/completions`、单 prompt `/v1/completions`。gRPC 指 `sgl-model-gateway` 当前 Scheduler client 路径，覆盖 generate/chat，需要外部 client 和 servicer 的真实协议适配；本系列不新增 gRPC completions，也不迁移 `sglang.runtime.v1` 新 runtime。

首版不启用 LoRA、speculative decoding、多 TP/DP/PP、多 TokenizerManager、PD role switch、PDMux、持久 session、HiCache/外部 KV 存储、KV offload、memory-saver 权重换入换出、自定义/未审计的权重 loader 或执行插件、其他修改权重的更新路径。相关配置在启用 guard 时明确拒绝，不保持“看起来开启但某条路径绕过”的状态。

不支持的生成入口在 guard Worker 上也必须拒绝，不能只在 Gateway 拒绝。纯健康读取、元数据读取和经核对的只读管理操作不受此限制。

## 4. 配置与发布身份

### 4.1 新增配置

```text
Worker:
  --enable-pd-version-guard
  --model-uid <canonical-model-uid>
  --weight-version <immutable-release-id>   # 已有参数，guard 增加语义校验
  --pd-version-drain-timeout <seconds>      # 新参数，建议初值 60 秒

Gateway（PR 2）:
  --pd-version-consistency strict
  --pd-model-uid <canonical-model-uid>
```

除已有 `--weight-version` 外，上述 Worker 参数是设计草案。新字段应接入当前 arg groups 的定义和校验体系，不通过修改只读启动记录发布运行时状态。

未启用 Worker guard 时完全保留 legacy 行为。guard 是进程生命期内的执行模式，首版不支持运行中将其关掉。

### 4.2 ExecutionKey

```text
ExecutionKey = (model_uid, weight_version)
```

一个 UID 内固定模型架构、Tokenizer、模板和其他输入语义。切换这些内容需要新的 UID 和部署，不是一次同 UID 的权重更新。

首版受保护的 UID 和版本采用 1–256 字节、无空白的可打印 ASCII 标识，精确比较，禁止 `default` 版本。已有自由格式标签仍可在 legacy 使用。版本字符串不表示大小关系。

发布方负责 key 到 checkpoint 产物映射不变。建议记录固定模型 revision 或 checkpoint manifest digest，并在实验中验证实际文件内容；不在每次生成请求上计算全量权重哈希。

### 4.3 对外 ExecutionInfo

```json
{
  "schema_version": 1,
  "key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
  "instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116",
  "publication_seq": 3,
  "state": "ready",
  "version_guard": "drain_v1"
}
```

实例 ID 在进程重启时改变。`publication_seq` 在每次可见发布状态转换时递增，包括 drain、恢复旧 Ready、失败和成功发布；不能只在权重更新成功后增加。

`execution_info` 从同一个不可变发布快照读取，不能分别读若干可变字段后拼成一个互相矛盾的对象。HTTP 顶层 `weight_version` 与其保持一致。Updating/Unavailable 阶段的 key 表示最后已提交发布，不表示半更新中的内存权重可以服务请求。

## 5. 版本约束与接纳回执

### 5.1 请求形状

所有受保护的外部生成请求必须携带：

```json
{
  "execution_constraint": {
    "protocol": "drain_v1",
    "expected_key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
    "target_instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116"
  }
}
```

`target_instance_id` 可省略，表示只要求正确发布 key；Gateway 正常请求会填写选定实例。内部 rebootstrap 可不固定实例，但必须保留 expected key。Worker 不从自己的“当前版本”补齐外部请求缺失的 expected key。

HTTP 各入口将该对象传入共享生成输入；gRPC 从 `sglang-execution-constraint-bin` metadata 解码同一对象。大小和类型校验与 PR 1/2 一致；未知协议不按 legacy 解释。

### 5.2 接纳回执

```json
{
  "protocol": "drain_v1",
  "accepted_key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
  "instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116",
  "publication_seq": 3
}
```

回执由实际接单快照生成。HTTP 在 `X-SGLang-Execution-Receipt` 中传无填充 base64url JSON；gRPC 在初始 metadata `sglang-execution-receipt-bin` 中传 JSON bytes。解码后上限 4 KiB。

成功 receipt 表示已建立执行保护，不表示 transfer 或整个生成完成。不能通过原样 echo 请求约束实现它。

## 6. 本地执行保护组件

建议新增小型共享组件 `ExecutionGuard`，接入 TokenizerManager 和 Scheduler；Python 数据容器按当前约定使用 `msgspec.Struct`。不要把整个协议展开成两个大类里大量松散字段。

### 6.1 状态与所有权

```text
PublicationSnapshot:
  key, instance_id, publication_seq, state, internal_epoch

AdmissionLease:
  lease_id, key, epoch, rid, admission_seq,
  phase = Reserved | Sent | Admitted | Aborting | Retired

ExecutionGuard:
  immutable publication snapshot
  short-lived admission/state mutex
  one administrative update mutex
  in-flight lease registry
  drain/retirement notifications
```

这是组件契约，不是现有类名。lease 保护本地 epoch，并不预留另一个 Worker 的容量，也不是有期限就能强行释放 GPU 内存的租约。

前端持有接单/预处理阶段的 reservation；一旦请求可能到达 scheduler，其释放依据变为 scheduler 和传输层的实际终止确认。输出消费者、HTTP handler 或 gRPC context 的销毁不能单独释放该 lease。

### 6.2 接单的原子边界

在一个短临界区内执行：

```text
验证 protocol 和约束完整性
读取当前发布快照
要求 state == Ready
要求 expected key == published key
如声明 target instance，要求实例一致
创建绑定该 epoch 的 lease，并登记在途引用
离开临界区
```

只有之后才能 tokenize、构造 `TokenizedGenerateReqInput`、查询可复用 KV、初始化 P/D transfer 或将请求送入调度器。

Ready 检查和 lease 登记不可分开。否则 drain 可能在两者之间认为没有在途请求并开始更新。

lease 建立后，即使状态变为 Draining，这个已经接纳的旧 epoch 请求仍可继续完成预处理和入队；不能在 writer lock 已等待时阻塞它的入队，导致更新等待它、它又等待更新的死锁。

### 6.3 与现有 reader/writer lock 的关系

legacy 保持当前 `generate_request` 的锁生命周期。strict 分支可让既有 reader lock 只覆盖需要一致模型视图的预处理/提交临界阶段，实际执行期间由 lease 保护 epoch；不因客户端慢读而无限延长权重保护。

更新必须先关闭接单并等待 lease/fence 收敛，再获取 writer lock。不能先持有 writer lock，再等待尚未完成 tokenization 的已接纳请求。

这里只调整显式 guard 分支，不以重构旧 RL 暂停路径为前置条件。

## 7. Scheduler 接纳、终止与 drain fence

### 7.1 接纳确认

`GenerateReqInput -> TokenizedGenerateReqInput -> Req` 保留约束和本地 lease 标识。Scheduler 在任何缓存 lookup、P/D 队列插入或计算前核对 key/epoch，并通过现有返回通道发送新增的 `ExecutionAdmissionAck`。

ACK 带本地 lease ID 和实际接受的 key/epoch。前端确认后才发出 receipt。拒绝 ACK 则释放该次 reservation，并返回版本、实例或状态错误；不得先入队再报告拒绝。

若外部 gRPC RequestManager 直接投递 scheduler，它也必须经过同一个 guard 契约和 ACK 通道；只在 HTTP TokenizerManager 加锁不能覆盖这条路径。

### 7.2 终止确认

为已发送请求增加一次性的 `ExecutionRetired` 通知，或复用能够表达同等事实的现有完成通知。通知必须发生在本地计算、KV 引用和传输清理安全结束之后，携带 lease ID 与 epoch。

前端按 lease ID 幂等处理：重复通知不会重复减计数，旧 epoch 的迟到通知不能释放新的 lease。不能仅按可重用 RID 或 logical request ID 做减计数。

| 阶段 | 允许退休的条件 |
|---|---|
| Reserved、尚未发送 | 明确未发送的校验/tokenization 失败或取消，可本地释放 |
| Sent、入队状态未明 | 保留保护；等待拒绝、终止 ACK 或受控实例失败 |
| Scheduler 等待队列/grammar queue | 从队列移除并完成相关清理后退休 |
| Chunked prefill / overlap result | 最后一个可能引用该请求的结果被处理后退休 |
| P 的 KV 发送 | transfer 完成或安全失败，sender/metadata buffer/tree 引用释放后退休 |
| D 的 KV 接收与 decode | receiver、延迟清理、retracted 队列及生成状态结束后退休 |
| 客户端断连/abort | 发出取消不等于退休；同样等待实际终止条件 |

发送函数抛异常不一定意味着消息未到达。无法证明“尚未发送”时走 Sent 的保守路径，不在通用 `except` 中无条件释放保护。

### 7.3 drain fence

现有 `is_fully_idle()` 已有丰富检查，优先复用；本 PR 增加的是 **接单水位与传输安全完成之间的 fence**。[^S3]

开始 drain 时记录最后已接纳的本地序号，并向 scheduler 发送 `ExecutionDrainBegin(epoch, cutoff)`。Scheduler 继续处理该 epoch 下已接纳、序号不超过 cutoff 的请求；不接受没有合法接单记录的绕行生成。

等前端预处理/发送 reservation 和实际执行 lease 均收敛后，发送 `ExecutionDrainFence`。Scheduler 在正常事件循环中检查：`is_fully_idle(for_health_check=False, ignore_waiting=False)` 成立，待处理结果与控制消息水位已覆盖 drain 之前的请求，且 transfer backend 的相关引用确已结束，才回复 fence ACK。

CPU 队列清空也不自动等于 GPU 工作完成；需要以相关计算流事件和 transfer backend 的完成语义确认旧工作不再访问权重或 KV。等待 fence 不能阻塞 scheduler 主循环；保存待确认 fence，在后续调度/清理 tick 中检查。不能仅查看健康检查用的 idle，也不能用 `_engine_paused` 导致的 `ignore_waiting=True` 作为 strict 更新条件。

收到 abort ACK 若只表示“取消命令已收到”，不满足 fence。若后端无法证明已提交的 DMA/传输不再引用旧缓冲区，保持不可更新，必要时由受控重启恢复，不能超时后直接释放并复用内存。

## 8. 流式入口的接入

现有生成 async iterator 可能在实际迭代时才开始 normalize 和提交。strict 入口需要先完成 guarded admission/ACK，再构造成功流式响应和 receipt；不能一创建 `StreamingResponse` 就无条件发出带 receipt 的 200。

可把接单结果封装为 `AdmittedGeneration`，其中包含 receipt、输出 iterator 和请求取消句柄。HTTP/native/OpenAI adapter 共享它，避免每个端点各写一套校验。

Scheduler ACK 后、真正产生输出前出现失败，按现有生成错误通道传播。Gateway 决定是否已对客户端提交；Worker 的 receipt 不自动使 Gateway 进入 committed 状态。

客户端断连时调用取消句柄，但 lease 仍由第 7 节的实际退休路径管理。Tokenization 尚未完成的断连应结束 reservation；已发送的请求则等待实际清理。

## 9. 原地更新协议

### 9.1 状态转换

```text
Ready(v10)
  -> Draining(v10)
  -> Updating(last_committed=v10)
  -> 加载完整 checkpoint，清理缓存，提交新 key
  -> Ready(v11)
```

允许多个 Worker 分别 drain，不要求 P、D 同时更新。只要仍有旧版本完整 pair，Gateway 就可以继续把相应请求交给它；没有容量时返回不可用，不跨版本补齐。

### 9.2 具体步骤

**第一步：检查更新请求。** 验证目标版本、固定模型语义配置、checkpoint 来源和受支持 load format；目标 key 与当前 key 相同的原地覆盖直接拒绝。strict 要求 `flush_cache=true`，首版更新不接受 `abort_all_requests=true`；需要取消旧请求时由管理方显式先 abort，再等待真实退休。

**第二步：获取单个更新操作的所有权。** 同一 Worker 同时只能有一个更新操作。新操作遇到已有 Draining/Updating 时返回冲突，不并行加载两份 checkpoint。

**第三步：关闭新接单。** 在 admission/state mutex 内将 Ready 改为 Draining，发布新的序号，记录 cutoff。已经接纳的旧请求继续运行；此时不能设置使它们停止前进的全局 pause。

**第四步：等 lease 和 fence。** 等待旧 epoch 的所有 reservation、执行及 transfer 退休，获得 scheduler fence ACK，再获取已有 writer lock。等待阶段不持有 admission mutex。

**第五步：独占加载与缓存失效。** 发布 Updating，调用已有完整 disk loader；权重写入、必要的 graph 处理、引擎缓存清理及新版本准备都在独占阶段内完成。不能先对外修改版本字符串，再慢慢更新权重。

**第六步：本地提交。** Scheduler 提交新执行 key 并确认，在 scheduler 与前端都确认新 epoch 可接单之前，前端 gate 保持关闭。前端随后一次性发布新 `ExecutionInfo` 和顶层版本，状态切回 Ready。发布和 gate 开放共用状态临界区。

Scheduler 暂时先拥有新 key 而前端尚未开放的窗口是允许的；它不能接受绕过合法 admission 的请求。确认丢失时保持不可用并进行状态核对，不根据猜测开放 gate。

### 9.3 超时、异常和管理客户端断连

默认 drain timeout 建议 60 秒，可配置。若超时发生在任何权重写入之前，终止这次更新，经本地确认取消 drain 后恢复旧 Ready，递增状态序号；旧请求继续按旧 key 执行。确认过程失败则保持 Unavailable。

一旦开始修改权重，失败进入 Unavailable。不能仅把标签改回 v10 就宣布回滚成功；需要完成一份已知完整 checkpoint 的恢复或重启，再发布 Ready。首版默认采用受控重启恢复 Unavailable，不另外开放一个绕过 drain 的在线“强制恢复”入口。

更新操作的所有权不依赖发起更新的 HTTP 连接。开始修改后的任务受组件管理，客户端断连不能释放 writer lock 或让 gate 自动开放；正常完成后写入结果状态，异常时保持不可用。管理方可查询 `/model_info` 的状态和版本判断结果，不能对超时响应盲目重复覆盖同 key。

本 PR 不新增全局发布数据库或权重事务协调器。上述提交只发生在一个 Worker 的进程和 scheduler 之间。

### 9.4 回滚模型

回滚是重新加载该 UID 下以前发布过的不可变 checkpoint，例如从 v11 drain 后恢复 v10。它是一个新的本地 publication epoch，实例内序号继续递增，但执行 key 可以是旧的 v10。

同 key 的内容不得改变。请求绑定不会因回滚重新解释；已绑定 v11 而无 v11 容量的请求仍然失败，而不是被强行迁移到 v10。

## 10. 缓存与传输隔离

首版更新只在完整 idle/fence 后清理引擎的 device KV、Radix/prefix cache 及相关请求池和元数据状态。现有 `flush_cache()` 使用 `_engine_paused` 决定是否忽略 waiting；guard 分支必须先完成严格 idle，不能把这个既有快捷条件当成安全证明。[^S10]

Gateway 的缓存提示失效由 PR 1 的实例/key 变化通知处理。它不是引擎 KV 清理，不能以 Gateway 的树已清空代替 Worker 的 cache flush。

P 完成并安全交付的 KV 已由 D 在相同 key 下持有；P 此后更新不必阻止 D 继续运行。D 的 lease 保护它自己的权重与 KV，直至本地结束。相反，P 仍在发送时不能更新并复用其缓冲区。

跨 Gateway attempt 使用新 RID/room，不将上次失败的会话重新用于新 pair。网络超时和取消可能留下短暂旧工作，但其 lease 会继续阻止旧 Worker 不安全更新。

HiCache、外部 KV store、offload 和跨进程持久 session 首版禁用。将来扩展时应定义 key 命名空间、存储失效和所有异步 I/O 的退休条件，不能只在当前 hash key 里附加一个版本字段就宣告完成。

## 11. 防止更新和管理入口绕过

启用 guard 后，HTTP 管理入口与 Scheduler 控制分发都必须实施同一操作范围，不能只在外层 route 加判断。

| 操作 | 首版 guard 行为 |
|---|---|
| `update_weights_from_disk` | 仅允许本 RFC 的完整 drain 更新 |
| `/update_weight_version` / `UpdateWeightVersionReqInput` | 拒绝单独发布执行版本 |
| tensor / distributed / IPC 权重更新 | 拒绝，返回未受保护的更新路径错误 |
| `begin_weight_update` / `end_weight_update` | 拒绝接入 legacy 更新 session |
| `pause_generation` 的 in_place/retract | guard 模式拒绝该跨更新恢复入口；legacy 不变 |
| 显式 abort 请求 | 允许，但不以“命令收到”代替退休 |
| LoRA 加载/卸载、动态 role switch、PDMux | 启动与运行时入口均拒绝 |
| 包含权重的 release/resume memory | 拒绝；首版不支持此模式 |
| 可能改写模型/执行配置的 `set_internal_state` | 拒绝受保护字段变更 |
| 通用动态 RPC | guard 首版拒绝未列入只读白名单的调用 |
| model-info、liveness、经核对的只读诊断 | 允许 |

本次源码中 `SchedulerWeightUpdaterManager` 已集中处理多种权重更新和 begin/end session；优先在该组件加入共用 guard，而不是在每个 HTTP 端点复制全部状态机。[^S9]

同样，当前 Scheduler 的动态 RPC 能按方法名调用成员函数，受保护配置不能留下这一条绕行通道。该限制只作用于显式 guard Worker，不调整 legacy 管理接口的整体能力。

任意恶意插件、进程外直接篡改 GPU 内存等不属于该协议可防御的边界；部署方仍须控制代码、凭据与管理权限。首版的“不可绕过”指支持配置内所有正常生成和管理入口均受相同契约约束。

## 12. 内部 rebootstrap

### 12.1 传播绑定 key

当前 `Req.build_rebootstrap_payload` 以已有 input/output IDs 和 bootstrap 参数构造原 P 的 `/generate` 请求。新增 `execution_constraint` 必须由 `Req` 保存的原始 expected key 构造，不能在重建时读取 D 或 P 的最新全局版本。[^S7]

发送请求后，`submit_prefill_recompute` 的 HTTP 执行路径同样验证接纳回执。只检查 HTTP 2xx 不够，因为旧 P 可能忽略新字段。[^S8]

### 12.2 恢复语义

原 P Ready 且仍为正确 key 时，按新本地 lease 执行重算；原 P 已更新、处于 Draining、缺少协议或不可用时，通过现有 `KVPoll.Failed`/abort 路径使本次请求明确失败。

不为失败 rebootstrap 自动寻找另一个版本；也不新增一个独立的 P failover 控制器。内部重建仍遵守既有 room/channel 重建规则，不盲目应用 Gateway “每次 retry 新 room”的规则。

D 已接纳的旧 epoch 请求在 D 自身 Draining 期间可继续做必要的同版本重建，但不能因此绕过 P 的新接单 gate。P 不再接受时允许失败，这不是保证零中断的全局更新事务。

### 12.3 gRPC Worker 的 HTTP 重建入口

本次 Python `grpc_server.py` 的 sidecar 主要提供 metrics/profile，并不是现成的 `/generate` 入口。`common/conn.py` 则依赖 bootstrap 中的 `prefill_http_port` 来发送重建请求。[^S8][^S11]

因此，gRPC strict 的验收还必须满足其内部恢复链路。首版采取以下限定：提供受相同 guard 保护的 `/generate` sidecar 入口，并让 bootstrap 发布真实 sidecar HTTP 端口；它调用同一请求管理器和 lease/ACK 通道，使用相同认证边界，不启动第二个独立接单所有者。

不能把 gRPC 主端口当成 HTTP generate 端口。若外部 RequestManager 尚不能支持这条桥接或恢复链路，gRPC 的 `drain_v1` 不激活；HTTP strict 可以独立完成验收和交付。不能用一个“只有小请求成功”的 gRPC 冒烟测试替代这一要求。

## 13. HTTP、gRPC、健康检查的能力激活

### 13.1 HTTP

`/model_info` 返回真实运行时快照；所有范围内生成入口共享 `AdmittedGeneration`。guard Worker 上缺约束的外部生成请求返回明确错误，不默认用当前 key 执行。

OpenAI 的 model 字段仍按原规则验证。执行约束不能冒充模型授权、LoRA 选择或用户 metadata。Responses 等未接入入口在执行前拒绝。

### 13.2 gRPC

当前 Python 入口使用外部 `smg_grpc_servicer`，Gateway 使用外部 `smg-grpc-client`。它们的源码未完整包含在本次 ZIP，不能根据本地 wrapper 宣称已经实现下列行为。[^S11][^S12]

启用 gRPC guard 的依赖交付应包含：GetModelInfo 动态发布 metadata；Generate request metadata 解码；共享 guard/lease 的接单和终止处理；初始 receipt metadata；精确拒绝 status；客户端保留 metadata；第 12.3 节的 HTTP rebootstrap 桥接与端口注册。

依赖应固定到真实可用的发布版本，并列出对应源码提交和集成测试。元数据键只是设计约定，不意味着现有包会自动支持。启用 `--enable-pd-version-guard` 且 adapter 不支持时启动失败，错误说明缺少哪项能力；未启用时保持 legacy。

### 13.3 健康和 warmup

liveness 与 readiness 分离。Draining/Updating 不应因主动退出接单就被判为 GPU 故障，但 readiness 不应宣告可接新 strict 请求。

启动 warmup 在初始化独占阶段执行，公共 gate 保持关闭；若复用 generate 路径，使用仅进程内部能创建的初始化执行句柄，不从外部 header 或特殊 RID 构造。warmup 完成后才首次发布 Ready。

正常 generation health probe 进入模型时使用本地当前 key 的普通 lease；Draining/Updating 时不能作为新生成继续接纳。更新后的必要 warmup 同样在更新独占阶段执行，归属于尚未对外开放的新 epoch，不冒充旧 key 的探针。

strict 首版不接受通过 fake bootstrap 参数逃逸正常 P/D 校验的外部请求。已有内部测试/探针保留专用受控路径。

## 14. 错误与生命周期语义

| 情况 | HTTP / gRPC | 资源语义 |
|---|---|---|
| 缺约束、损坏约束、未知协议 | 400 / INVALID_ARGUMENT 或明确协议前置条件错误 | 不创建执行请求 |
| key 不相等 | 409 / FAILED_PRECONDITION | 不查询 KV、不初始化传输、不运行模型 |
| 指定实例不相等 | 409 / FAILED_PRECONDITION | 不执行，允许 Gateway 刷新后同 key failover |
| Draining/Updating/Unavailable | 503 / UNAVAILABLE | 不接新请求 |
| 更新参数不支持、单独改标签、绕行更新 | 400/409 或对应管理错误 | 不开始权重修改 |
| drain 超时、尚未写权重 | 更新失败；确认恢复后旧 Ready | 旧请求继续，不强制覆盖 |
| 写权重后失败、fence/commit 结果不明 | Unavailable | 不仅靠标签恢复服务 |
| 已提交流的后续错误 | 终止当前流 | 清理完成后退休，不透明重跑 |

错误对象包含稳定 code 和阶段，不依赖解析自然语言 message。安全拒绝不标记为 GPU 推理故障；真正连接失败、引擎崩溃和传输错误单独处理。

Gateway 的 same-version retry 仍使用独立 attempt。Worker 不替 Gateway 做另一个无界 retry loop，也不通过默认当前版本替它“修复”失配。

## 15. 正确性说明

设一个成功 strict 请求的 bound key 为 K。

首先，PR 2 对 P、D 注入同一个 K，并验证两端接纳回执。每个实现 `drain_v1` 的 Worker 只有在当前已发布 key 等于 K 时才创建 lease，因此两个实际接单的执行 epoch 都属于 K。

其次，更新必须等所有该 epoch 的 lease 退休和 drain fence 成立后才写权重。请求的等待、计算、KV 发送/接收及内部重建不可能在该 Worker 已切换权重后继续被当成原请求执行。

再次，引擎旧 KV 在新 Ready 之前被清理；本地重建显式携带 K，并在原 P 上再次接纳校验。成功结果不能通过恢复路径混入其他版本 KV。

最后，Gateway 的整个尝试重试保持 K，并使用新 RID/room；客户端输出提交后禁止透明重试。因此成功交付的逻辑请求不会因路由、重试或更新而漂移到另一 key。

该论证以发布方诚实维护 key-to-checkpoint 映射、支持范围内没有未受保护的执行入口、transfer 退休条件真实有效为前提。它不证明任意相同标签的权重相等，不保证整个集群在同一时间运行同一版本，也不保证没有旧容量时请求仍可成功。

不需要 P/D 全局两阶段提交：一端拒绝时另一端可能浪费部分计算，但最终输出必须满足一致性。这里选择有界失败，而不是跨版本补齐。

## 16. 代码改动落点

| 位置 | 主要修改 |
|---|---|
| `arg_groups/fields/serving.py`、相关 hook/validation | guard、模型 UID、drain timeout 与范围校验 |
| `managers/execution_guard.py`（建议新增） | 本地发布快照、lease、drain 操作所有权 |
| `managers/io_struct.py` | 请求约束及 admission/retired/fence 消息 |
| `managers/tokenizer_manager.py` | guarded admission、输入传播、ACK/退休处理、更新入口接入 |
| `managers/tokenizer_control_mixin.py` | 更新 session、标签与管理入口的 guard 限制 |
| `managers/schedule_batch.py` | Req 保存 key/lease；rebootstrap payload |
| `managers/scheduler.py`、小型 scheduler component | 缓存访问前验证、非阻塞 drain fence、运行时管理限制 |
| `managers/scheduler_components/weight_updater.py` | drain 后的 disk 更新与独占提交；其他更新路径限制 |
| `disaggregation/prefill.py`、`decode.py`、`common/conn.py` | transfer 安全退休、重建约束与 receipt 校验 |
| `entrypoints/http_server.py`、OpenAI serving adapters | admission 完成后生成响应、运行时快照 |
| `entrypoints/grpc_server.py` + 外部 servicer/client | binary metadata、共享 guard、受控 sidecar generate |
| 现有 Gateway adapter 与配置 | 完成协议能力探测、严格验收后激活 |

避免在 ModelRunner 核心加载逻辑中重写一套发布系统。实际 disk loader 尽量复用；调度器可写状态和转发消息的变更按现有组件组织，而不是在初始化函数里堆叠所有逻辑。

## 17. 测试计划

### 17.1 无 GPU 的状态机与协议测试

| 编号 | 场景 | 必须断言 |
|---|---|---|
| G1 | Ready、key 相同 | 取得 lease，ACK 与 receipt 是实际 key |
| G2 | key/实例不相同 | 在 cache/queue/transfer 初始化前拒绝 |
| G3 | key 校验与开始 drain 同时发生 | 只有“先接纳并计入 drain”或“拒绝”两种结果 |
| G4 | 已接纳、tokenize 尚未结束时 drain | 不死锁，计入 reservation，不能提前更新 |
| G5 | 明确未发送与发送状态不明的异常 | 前者可释放，后者保留到确认 |
| G6 | 重复/迟到退休消息 | 计数幂等，不释放其他 epoch |
| G7 | 客户端断连但 scheduler/transfer 尚存活 | handler 结束不解除保护 |
| G8 | 旧 Ready 响应晚于 Draining 状态 | 序号递增、快照一致 |
| G9 | drain timeout、未写权重 | 确认取消后恢复旧 Ready，旧请求不漂移 |
| G10 | 加载中失败或 commit ACK 丢失 | Unavailable，不冒充旧 Ready |
| G11 | `flush_cache=false`、同 key 覆盖、单独改标签 | 写入前拒绝 |
| G12 | tensor/distributed/IPC/session/RPC 绕行 | 前端和 scheduler 两层均不能绕过 |
| G13 | 200 但缺失/错误 receipt 的旧 Worker | Gateway 不输出 strict 成功 |
| G14 | 外部伪造 health RID/fake bootstrap | 不绕过正常 admission |

测试使用可控 barrier 和假的 scheduler/transfer ACK，不依赖随机 sleep。每个测试明确断言是否触及模型执行或 KV 访问钩子，而不只检查最终错误码。

### 17.2 实际调度与传输生命周期测试

复用已有 disaggregation 单元测试模式，增加等待队列、grammar、chunked prefill、overlap result、P transfer、D receive、retracted queue 和延迟释放的场景。首版未启用的功能应测试启动拒绝，不用空 fixture 假装已验证。

在 `is_fully_idle` 已覆盖的队列之外，核对 admission reservation、尚未处理的转发消息和 backend transfer 引用。任何一项未安全退休时 fence 不通过。

对 abort/failure 注入使用已有 prefill/decode cleanup 测试作为回归基线，尤其检查 sender clear、metadata buffer、tree handle 和 receiver 的顺序。本次快照中可复用的文件包括：

```text
test/registered/unit/disaggregation/test_admission_abort_not_enqueued.py
test/registered/unit/disaggregation/test_prefill_abort_result_cleanup.py
test/registered/unit/disaggregation/test_decode_queue_cleanup.py
test/registered/unit/disaggregation/test_deferred_decode_kv_release.py
```

### 17.3 内部 rebootstrap

至少测试原 P 同 key 成功、原 P 更新后拒绝、原 P Draining、原 P 不可达、原 P 返回 200 但无 receipt。检查直接 `/generate` 的实际 payload，并确认 D 的失败由既有 transfer failure 路径收敛。

gRPC 必须测试 sidecar HTTP 端口的实际 bootstrap 注册和重算，不能仅手工向 sidecar 发请求。确认 sidecar 与普通 gRPC 请求共用一个 guard，而不是两个互不知晓的计数器。

### 17.4 GPU 端到端

使用固定 revision 的 Qwen3 小模型，准备同构但真实权重不同的 A/B checkpoint。记录来源、文件 manifest/digest、dtype、模型配置、SGLang 提交及 transfer backend 配置。只改 `weight_version` 标签不足以验证实际权重一致性。

基本正确性至少需要一台 P 和一台 D；验证同版本 failover 还需要同角色替代实例，可用独立端口及受控资源分配实现。不能将仅有唯一 P、D 的实验描述为已经验证完整冗余 failover。

| 场景 | 验收结果 |
|---|---|
| A/A、B/B 静态 pair | 相应版本的正常基线 |
| 只有 P(A)、D(B) | Gateway 在 dispatch 前拒绝 |
| 两个完整版本组 | 每个成功请求的 P/D actual key 相同 |
| Gateway 选中 A 后，D 在 admission 前切成 B | D 拒绝；只允许 A 内重试或失败 |
| 长 decode 期间更新 D | 更新等待旧请求实际结束，不产生跨 epoch token |
| P transfer 期间更新 P | writer 等安全退休，不与缓冲区复用重叠 |
| 客户端断连后发起更新 | 取消收敛前不能进入权重写入 |
| B 发布失败 | Worker 不重新宣告 Ready |
| B -> A 回滚 | 新 publication seq，内容仍为原 A |
| 返回部分流后 Worker 失败 | 当前流结束，无隐藏新尝试拼接 |

通过 scheduler/loader instrumentation 核对 actual key 与 checkpoint provenance。数值对比使用同版本基线和合理容差，不以跨 batch/不同内核的 bitwise 相同为要求。答案文本相同既不是跨版本错误的反证，也不是版本保护正确的证明。

HTTP 各入口分别测试；gRPC 使用实际安装的 client/servicer 分别测试。未通过的传输不开放 guard capability。

## 18. 性能与运维验证

steady-state 新增成本主要为本地 admission 临界区、lease registry 操作和 ACK/退休通知。Gateway 不增加逐请求 model-info RPC，但 Worker 的新控制消息有成本，必须实测，不能把它写成“零开销”。

报告无更新时的吞吐、TTFT、TPOT、请求路径 CPU 与内存；更新时报告 gate 关闭到 drain 完成、加载、cache flush、本地 commit 和恢复 Ready 的分段时延，以及主动拒绝数量和同版本重试次数。

使用相同硬件、checkpoint、负载、sampling 和 legacy/strict 配置做对照。版本失配导致的拒绝是预期正确性行为，不从结果中偷偷删掉以美化可用率。

监控 lease 数、最老未退休年龄、drain 阶段、更新拒绝、未知执行状态、receipt 拒绝与传输清理失败。版本和 RID 放 trace/log，不直接用于高基数指标 label。

滚动升级顺序为：先部署支持 guard 的 Worker，但在完整验证前不接 strict 流量；再启用 Gateway strict。回滚 Gateway 仅影响新请求，guard Worker 的 gate 与在途 lease 仍有效。若回滚为不认识约束的旧 Worker，必须先撤流并排空，不能在同一个 strict 地址后静默换旧二进制。

## 19. 合并与激活标准

PR 3 的 HTTP 激活条件是：所有范围内入口传播正确；真实 scheduler admission/retirement/fence 已接入；disk 更新和其他管理路径不能绕过；内部 rebootstrap 受同一 key 约束；GPU 更新竞态、断连清理和基本同版本恢复通过。

gRPC 在此基础上，还必须完成真实外部依赖的 request/response metadata、共享接单所有权、sidecar rebootstrap 与正确端口注册。缺少任一项时保持该传输不可激活，不能用 HTTP 的测试结果替代。

建议实现提交依次为公共结构和 guard；HTTP admission/ACK；Scheduler 退休/fence；disk publication 与更新入口限制；内部 rebootstrap；gRPC 依赖接入；集成/GPU 测试和文档。它们属于一个完整安全契约，未闭合阶段默认不开启功能。代码量过大时可以按这些提交另拆实现 PR，但不能提前发布完整 strict 承诺。

本次文档编写只完成源码审阅和设计，不包含 Rust 构建、Python 集成或 GPU 执行结果。实现 PR 应补实际命令、提交、环境与测试结果，而不是沿用这里的验收目标作为结果。

## 20. 源码依据

[^S1]: `python/sglang/srt/managers/tokenizer_manager.py:854–929`，现有 normalize、reader lock 和生成生命周期。  
[^S2]: `python/sglang/srt/managers/tokenizer_manager.py:2163–2201`，paused 更新分支、writer lock 与标签更新时点。  
[^S3]: `python/sglang/srt/managers/scheduler.py:4993–5057`，现有完整 idle 检查。  
[^S4]: `python/sglang/srt/disaggregation/prefill.py:1047–1115`，KV transfer 完成与后续清理。  
[^S5]: `python/sglang/srt/entrypoints/http_server.py:1471–1495`，单独更新版本标签。  
[^S6]: `python/sglang/srt/managers/tokenizer_manager.py:2116–2162` 和 `scheduler.py:5579–5650`，abort/pause/retract。  
[^S7]: `python/sglang/srt/managers/schedule_batch.py:2059–2115`，内部重建 payload。  
[^S8]: `python/sglang/srt/disaggregation/common/conn.py:794–886, 1119–1144`，HTTP 重建和 prefill 端口注册。  
[^S9]: `python/sglang/srt/managers/scheduler_components/weight_updater.py:117–154, 290–370`，disk 与 session 管理。  
[^S10]: `python/sglang/srt/managers/scheduler.py:5169–5199`，cache flush 的 idle 参数。  
[^S11]: `python/sglang/srt/entrypoints/grpc_server.py:1–13, 73–82, 157–176`，sidecar 与外部 servicer。  
[^S12]: `sgl-model-gateway/Cargo.toml:114–118`、`python/pyproject.toml:81`，实际外部依赖声明。  
[^S13]: `python/sglang/srt/managers/scheduler.py:5369–5394`，动态 RPC 与版本标签控制入口。
