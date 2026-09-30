# RFC / PR 2：兼容 P/D 配对与请求级版本绑定

**建议 PR 标题：** `feat(gateway): add compatible PD pair planning and revision-bound retries`

**状态：** Draft。本文给出完整设计与验收条件，不包含已完成测试的声明。  
**基线：** 本次上传快照 `3ed56a341b57eb57c8e37e0cec353e67ebb35c3b`，实现分支叠加 PR 1。  
**前置依赖：** PR 1 的执行元数据快照、候选子集契约及策略修复。  
**后续依赖者：** PR 3 的 Worker 接单保护与受控发布。

## 1. 摘要

当前 HTTP 与 gRPC Gateway 分别从 P、D 池选择 Worker，缺少 pair-level 的模型发布约束。因此，在混合版本池中，两个分别合法的选择可能组成 `P(v10) + D(v11)`。[^S1][^S2]

本 PR 引入共享 `PdPairPlanner`：先根据可用 D 的执行 key 排除没有搭档的 P，再在合法 P 集合中运行原策略，最后在该 P 的 key 对应的 D 集合中选择 D。完整 pair 形成后，在发送任何请求前冻结逻辑请求的 key；后续重试只能在该 key 内重新运行整个 P/D 尝试。

Gateway 的判断基于元数据快照，不是对权重的 reservation。本 PR 同时定义并消费 Worker 的期望版本约束与接纳回执；PR 3 才实现真实 Worker 保护。在未具备能力的 Worker 上不降级成“只比较标签”的 strict。

## 2. 当前行为与稳定反例

### 2.1 现有选路

HTTP `select_pd_pair` 分别调用 P policy 和 D policy；非 IGW 模式还会将选路用的 model filter 置为 None。gRPC 先按连接类型和模型过滤，再分别对两个角色调用同一解析得到的 policy。[^S1][^S2]

HTTP retry closure 每次重选 pair；gRPC 每次 retry 重新执行 pipeline。gRPC 的 `DispatchMetadataStage` 从选中的 D labels 读取版本，用于响应元数据，不是 retry 外层的不可变请求绑定。[^S3][^S4][^S5]

### 2.2 两个反例

```text
案例 A：错误组合
P = [P0(M, v10)]
D = [D0(M, v11)]
两端分别选择唯一候选，得到跨版本 pair。

案例 B：先选 P 再检查造成错误失败
P = [P0(M, v10), P1(M, v11)]
D = [D0(M, v11)]
先让缓存策略选 P0，再寻找 v10 的 D，会失败；但 P1+D0 明明可用。
```

案例 A 需要禁止跨 key 配对；案例 B 要求在调用 P 策略之前排除无搭档候选。二者不能只靠“选完后检查一次”同时解决。

## 3. 首版范围

首版 strict 面向一个显式配置的规范模型域，Dense text、无 LoRA、无持久会话，验证 fixture 为固定 revision 的 Qwen3 小模型。每个 Worker 的 TP、DP、PP 均为 1，单 TokenizerManager 或等价的单接单所有者，固定同构 Tokenizer、模板和 Mooncake 配置。多个版本可以同时存在于该域。

这里的 HTTP/gRPC 指 Gateway 到 Worker 的链路，不表示新增了 Gateway 对外的 gRPC API。

| 客户端入口 | HTTP P/D 上游 | 当前 GrpcPDRouter 上游 |
|---|---|---|
| `/generate`，单样本 | 接入 strict | 接入 strict 的设计范围，需真实依赖支持约束/回执 |
| `/v1/chat/completions`，文本 | 接入 strict | 接入 strict 的设计范围，需真实依赖支持约束/回执 |
| `/v1/completions`，单 prompt | 接入 strict | 本系列不新增该路由 |
| `/v1/responses`、其他生成入口 | strict 明确拒绝，legacy 不变 | 不新增 |

当前 `GrpcPDRouter` 的 `RouterTrait` 实现仅覆盖 generate 和 chat，不能把 completions 也当成已有 gRPC P/D 能力。[^S6]

首版只接受单样本、`n=1`、非 speculative 的文本请求；支持流式与非流式。批量输入、多 choice、LoRA、Responses 工具续轮、引擎 session、跨 Agent 多次调用的 trajectory pinning、hedging、decode-only KV 迁移不在范围内。strict 对这些形状在 dispatch 前返回明确错误，不能绕回 legacy。

## 4. 身份与安全不变量

### 4.1 冻结的内容

```text
ExecutionKey = (model_uid, weight_version)
```

key 是模型发布身份，不是 Worker URL。`model_uid` 由部署配置确定；`weight_version` 是该模型域下不可变的发布标识。首版受保护的 UID 和版本均为 1–256 字节、无空白的可打印 ASCII 标识；只做精确比较，不排序，不使用 `default`、空字符串或未知值。

PR 1 提供以下 Worker 观察：

```text
ExecutionInfo {
  schema_version: 1,
  key,
  instance_id,
  publication_seq,
  state,
  version_guard: "drain_v1"
}
```

每次发布状态改变，`publication_seq` 都递增；`instance_id` 与序号用于快照和实例校验，不要求 P、D 相等。

### 4.2 三项不变量

**配对不变量：** 任一被发送的 strict attempt，P、D 的计划 key 均等于逻辑请求的 bound key。

**绑定不变量：** bound key 在第一次完整 pair 形成、任何 upstream dispatch 之前设置一次，之后不随重试、元数据刷新或发布默认值变化。

**成功响应不变量：** Gateway 只把具有两端有效接纳回执的结果作为 strict 输出。真正的执行 key 稳定性由 PR 3 的本地版本保护实现，不能由计划或响应指纹代替。

同版本重试不承诺逐 token 一致、exactly-once 计算或已经持有旧 KV。版本相等也不替代传输配置兼容性检查。

## 5. 配置与入口约束

新增配置建议为：

```text
--pd-version-consistency legacy|strict   # 默认 legacy
--pd-model-uid <canonical-model-uid>     # 首版 strict 必填
```

`strict` 只适用于 P/D 路由。首版一个 Gateway strict 配置绑定一个规范模型 UID；既有模型名解析和 IGW 过滤仍保留，不能用相同 UID 跳过请求的模型授权。未来多规范模型域可以将该配置扩展为 per-model 设置，本系列不引入新的模型别名控制器。

客户端可在 strict 请求上用新增 `X-SMG-Weight-Version` 指定版本；缺省采用 `any_compatible`。header 仅选择版本，不允许指定或覆盖 Worker 的 guard 能力、实例状态或规范模型 UID。值必须符合发布标识校验，不做隐式 trim 或新旧排序。

`any_compatible` 表示允许原 P 策略在所有拥有完整 P/D 容量的版本组中选择，并立即绑定结果。它不保证选最新版本、均匀分配版本或执行某个灰度比例。需要这些行为时，由发布方通过显式版本和容量管理控制。

strict 拒绝 `X-SMG-Target-Worker`，无论它是否恰好指向合法下标；这样避免过滤后数组变化改变用户指定目标的含义。`X-SMG-Routing-Key` 可以继续作用于合法候选，但不能绕过版本约束。legacy 保留现有 header 语义。

Gateway 生成的 `execution_constraint` 为内部字段。入口不接受用户用同名字段覆盖它；序列化 upstream 请求时始终由计划生成。相关内部回执 header 不接受客户端透传，避免混淆来源。

## 6. 共享规划器

### 6.1 数据结构

```rust
struct RequestRoutingState {
    bound_key: OnceLock<ExecutionKey>,
    requested_version: Option<String>,
    response_committed: AtomicBool,
    // 重试预算与逻辑 request ID 由同一个请求所有者持有
}

struct WorkerExecutionSnapshot {
    worker: Arc<dyn Worker>,
    execution: Arc<ExecutionInfo>,
    registration_generation: u64,
}

struct PdPairPlan {
    key: ExecutionKey,
    prefill: WorkerExecutionSnapshot,
    decode: WorkerExecutionSnapshot,
}
```

这些是设计类型，不要求逐字照搬。重要的是快照与 Worker 注册身份保持对应，且请求绑定不放在每次重建的 attempt context 中。

### 6.2 候选准入

候选必须同时满足请求模型范围、配置的 `model_uid`、正确 P/D 角色、当前 adapter 支持的 SGLang 连接类型、健康和熔断可用条件、有效且未过期的执行快照、Ready 状态以及 `drain_v1` 能力。

如果请求已经绑定，只允许 bound key；未绑定但显式指定版本时，只允许该版本。任一端缺元数据或协议能力都不进入 strict 候选，不将 `None == None` 解释成相等。

HTTP 路径也显式检查连接类型，不能因为 registry 存在同角色 gRPC Worker 就把它交给 HTTP dispatch。gRPC 保留现有连接和 runtime 过滤。

### 6.3 半连接算法

```python
# 说明算法，不是当前 SGLang 的 Python API。
def plan_pd_pair(prefill, decode, constraint, select_p, select_d):
    p_eligible = filter_eligible(prefill, constraint)
    d_eligible = filter_eligible(decode, constraint)

    d_by_key = group_by_execution_key(d_eligible)
    p_feasible = [p for p in p_eligible if p.key in d_by_key]
    if not p_feasible:
        raise NoCompatiblePair()

    p = select_p(p_feasible)
    require_member(p, p_feasible)
    d_candidates = d_by_key[p.key]
    d = select_d(d_candidates)
    require_member(d, d_candidates)

    require(p.key == d.key)
    return PdPairPlan(key=p.key, prefill=p, decode=d)
```

构造兼容集合的预期成本为 `O(|P| + |D|)`，另加既有策略成本。不构造 `P × D` 笛卡尔积，不逐 Worker 发 RPC，也不引入一套新的联合评分器。

HTTP 仍使用已配置的 P、D 分角色策略；gRPC 仍按原逻辑解析 policy，再作为两个角色的 selector 传入。共享的是兼容性与 binding 逻辑，不是顺便更换默认路由配置。[^S1][^S2]

### 6.4 策略与并发变化

PR 1 已保证子集选择不会越界，也不会删除其他版本的健康缓存提示。规划完成后检查选中的注册身份仍存在、健康/可用状态仍可接受；出现变化可在同一个有限预算中重新规划，不能扩展到全局池。

一旦完整合法计划已冻结 key，之后即使还未发送，也不自动换 key。冻结前的纯规划失败没有绑定，但不能形成无界循环。

策略内的估算缓存插入不是实际 dispatch。真实负载 guard、尝试次数和 Worker 请求指标在真正 dispatch 时更新；未发出的选择不能计为成功请求。该 PR 不要求回滚既有策略的每一个近似提示。

## 7. 请求绑定与尝试身份

### 7.1 绑定时点

逻辑状态在进入 retry executor 前创建。第一次得到完整 pair 时调用一次性绑定；冻结失败或与已有 bound key 不一致，视为内部契约错误，不发送请求。

不能等 Worker 首次成功响应后才绑定：网络超时可能发生在实际执行之后。也不能仅绑定 P 后再去发现有没有 D；案例 B 的预筛选应先完成。

### 7.2 三种身份分别使用

```text
logical_request_id：整次客户端调用，重试之间不变。
attempt_id / upstream rid：每次完整 P/D 尝试新建。
bootstrap_room：每次完整 P/D 尝试新建，P、D 使用同一个 room。
```

每次尝试使用一个独立随机上游 RID，P、D 两端共享它。客户端提供的 RID 不直接复用于多个 attempt；对外 request ID 与内部 RID 的映射由 Gateway 保留。

上游 RID 不采用会造成前缀碰撞的 `a1/a10` 简单字符串拼接。当前 scheduler abort 路径存在按 RID 前缀匹配的语义，重试标识必须避免一个尝试的取消影响另一个尝试。[^S12]

同一次引擎内部 rebootstrap 不等于 Gateway retry：它按现有传输协议处理 room 生命周期，不能由 Gateway 擅自换 room。PR 3 负责保留其版本约束。

### 7.3 gRPC preparation 也保持模型域稳定

gRPC pipeline 在 Worker selection 之前完成 preparation/tokenization。[^S7] 因此规范模型域及其 Tokenizer/模板映射必须在进入 preparation 前确定。

同一逻辑请求的输入解释保持稳定，retry 不重新解析成另一模型域。可以复用准备结果，也可以在固定配置下重新执行 preparation；不能在每次 attempt 从一个可变 alias 重新选择不同的 Tokenizer。首版要求该 UID 内固定 Tokenizer/模板，改变它们需要新的 UID。

## 8. Worker 约束与接纳回执协议

### 8.1 请求约束

HTTP upstream body 增加：

```json
{
  "execution_constraint": {
    "protocol": "drain_v1",
    "expected_key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
    "target_instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116"
  }
}
```

P、D 的 `expected_key` 相同；`target_instance_id` 分别取两端计划的实例。该字段不绑定整个逻辑请求的实例，failover 时可改变。内部重建可以省略实例限制，但不能省略 expected key。

gRPC 使用新增 request binary metadata：

```text
sglang-execution-constraint-bin = UTF-8 JSON bytes of execution_constraint
```

它需要 client 能设置 request metadata、servicer 能读取并传入真正的引擎请求；不通过 trace headers 偷渡，也不要求修改无关的 runtime proto。当前 wrapper 的 `generate()` 只返回 `ProtoStream`，所依赖的 client 若没有这些接口，必须先扩展依赖。[^S8]

### 8.2 接纳回执

Worker 在受保护边界内接受请求后返回：

```json
{
  "protocol": "drain_v1",
  "accepted_key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
  "instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116",
  "publication_seq": 3
}
```

HTTP 使用新增 `X-SGLang-Execution-Receipt`，值为上述 JSON 的无填充 base64url 编码；gRPC 使用 `sglang-execution-receipt-bin`，值为 JSON bytes。两种 adapter 归一化到同一回执类型，限制解码后大小不超过 4 KiB。

回执来自实际接单状态，不是收到请求后原样 echo expected key。它只证明接单和版本保护已经建立，不证明推理或 KV 传输已经完成。

Gateway 验证两端回执的 protocol、accepted key 和实例身份；同一实例的回执序号不能早于计划快照。序号无需 P、D 相等。缺回执、重复冲突回执或字段不一致均不得作为 strict 成功结果输出。

这一步封闭“旧 Worker 忽略未知请求字段但仍返回 200”的静默降级。不能把发现时曾经有 capability 当成这次请求一定执行了保护的证据。

### 8.3 公共响应与指纹

不把内部接纳回执直接当作面向用户的成功生成元数据。已有 gRPC `system_fingerprint` 或版本字段在 strict 中应取冻结的 bound key，不能在最终序列化时重新读取 D 的当前 labels；legacy 格式保持不变。[^S5]

真实输出的数值归因仍要靠 PR 3 的执行保护和测试。仅在响应中打印正确字符串不是验收标准。

## 9. HTTP 与 gRPC 的接入点

HTTP retry closure 外持有 `Arc<RequestRoutingState>`；`select_pd_pair` 的 strict 分支调用 planner。序列化原始业务请求、注入新的 room/RID，然后为 P、D 分别注入执行约束，保留既有 route-specific body 转换。

gRPC `route_generate_impl`、`route_chat_impl` 在 retry 外创建相同状态，并显式传给每次创建的 `RequestContext`。不能依赖 `ProcessingState` 自动跨重试保存，因为 pipeline 每次都会新建 context。[^S4][^S9]

request building 产生 pair 的共用业务参数和独立 attempt 信息；request execution 为两端生成各自的实例约束。不得在 clone 同一个 proto request 时误把 P 的目标实例也复制给 D。

HTTP 现有 dispatch 会保存 decode 的提前响应，但仍优先等 prefill 完成；gRPC 双 dispatch 使用 `join!` 等待两端启动。[^S10][^S11] strict 分支需要识别任一端的接单拒绝，及时取消另一端，避免 D 已拒绝而 P 一直等待传输握手。

P、D 仍并发派发。不能为方便读回执而改成“P 完全执行结束后才发 D”，因为 P/D 握手和传输可能需要两端同时存在。

## 10. 重试状态机

```text
Unbound
  -> PlannedAndBound
  -> Dispatching
  -> Accepted
  -> ResponseCommitted
  -> Finished

Dispatching / Accepted --可重试故障、尚未提交--> Cleanup -> PlanSameKey
任何状态 --终止错误/预算耗尽--> Failed
ResponseCommitted --后续错误--> 终止当前响应，不建立新尝试
```

### 10.1 可以重试的范围

传输失败、Worker 在接单前发生版本/实例变化或处于 Draining，可在剩余 deadline、重试次数和退避预算内，重新选择同 key 的完整 pair。版本/实例失配可触发一次合并元数据刷新。

每次重试重新执行 prefill 和 decode，不复用上一次的 room 或假定新 D 已拥有 KV。即使只有 D 出错，也不提供 decode-only continuation。

绑定版本没有任何完整 pair 时，立即返回 `bound_revision_unavailable`，不把该错误交给通用“所有 503 都重试”的逻辑反复轮询。需要等待某版本恢复的长轮询行为不在本 PR 中引入。

### 10.2 响应提交边界

内部 upstream HTTP 200 或 gRPC stream 建立，不等于对客户端已经提交。Gateway 可以在尚未向客户端输出时处理内部错误并重试。

一旦成功响应头或任何输出开始交给客户端，视为 committed。实现上可以在将成功的流式 `Response` 交给 axum 前保守地设置该状态，此后流处理器禁止建立新 attempt。不能把另一版本或另一尝试的 token 接在已有流后面。

### 10.3 清理与未知执行状态

每个尝试有独立取消所有者。一个 leg 拒绝或失败时，取消另一端的请求/stream，按独立 RID 发出已有 abort 或 transport cancellation。只取消网络 future 不能被记录为“引擎资源已释放”。

前次尝试执行状态不明时，可以用新 RID/room 做同 key 重试，但必须接受可能存在重复计算：旧尝试由引擎终止确认或其传输超时收敛。旧实例不能仅凭 Gateway 的 future 已释放就被认为允许更新；PR 3 的 lease/fence 负责该保证。

不新增 detached generation 任务。必要的取消收敛任务由 Gateway 生命周期管理，具有超时与退出清理，不无限保存请求体。若发现协议无法隔离尝试或无法给清理提供终止路径，该 adapter 不启用 strict。

## 11. 错误分类与熔断

新增类型化结果，例如 `PdRoutingError` 和 `AttemptFailure`，携带 `retry_disposition`、责任角色和是否已提交。不要通过错误字符串匹配或单一 HTTP status 推断重试策略。

| 情况 | Gateway 对外 | 重试行为 |
|---|---|---|
| header 格式错误、禁用的 target、超范围请求形状 | 400，明确参数代码 | 不重试 |
| 无兼容 pair | 503 `no_compatible_pd_pair` | 不等待版本出现 |
| 已绑定版本无完整 pair | 503 `bound_revision_unavailable` | 不跨版本，不轮询 |
| Worker 版本失配 | 内部 HTTP 409 / gRPC FAILED_PRECONDITION | 有限刷新并同 key 重试；耗尽后对外 503 |
| Worker 实例失配或非 Ready | 内部 409/503 或相应 gRPC status | 同 key 更换实例 |
| 缺少有效回执或不支持协议 | 502 `execution_contract_not_honored`，或候选期的能力不可用 | 隔离该实例的 strict 能力；不静默降级 |
| 真正的连接失败、上游超时 | 沿用 502/504 等传输分类 | 预算内同 key 重试 |
| 已提交后的错误 | 终止当前响应 | 不重试 |

版本失配、Draining 和合法的接单拒绝不计为推理硬件故障；也不计为 breaker 的成功探测。真实传输错误按责任 leg 记录，不能因为 P 拒绝就同时惩罚健康 D。

当前 HTTP 和 gRPC 代码已有 outcome 记录位置，需要保证 strict 分支只记一次。`BreakerOutcomesRecorded` 等既有去重机制能复用就复用，不顺带改变所有 legacy breaker 行为。[^S3][^S11]

## 12. 能力启用与向后兼容

默认模式为 legacy，其选路、重试和公开响应行为不变。PR 1 的明确 bugfix 除外。

PR 2 可以合并 planner、状态传递、HTTP 约束/回执 adapter 和 mock 验收，而不依赖 PR 3 的引擎代码编译。对于真实旧 Worker，缺少 `drain_v1` 时 strict 无候选，返回明确的能力不可用；不向这些 Worker 发送有执行保护承诺的请求。

gRPC adapter 必须有真实 request/response metadata 接口才能标记为可用。没有接口时报告该传输的 strict 能力不可用，不能仅因 client 能发普通 Generate 就放行。

因此，PR 2 单独部署时的生产价值是共享规划与已测试的请求语义准备；静态池 mock 可以证明路由性质，但不能替代执行一致性发布。直到 PR 3 完成某个传输的验收，才对该传输宣告端到端 strict 支持。

配置不能让进行中的 bound 请求降级。关闭 strict 只影响之后进入的新请求；模型回滚通过重新提供旧 key 的完整 pair 实现，不修改在途请求的 key。

## 13. 可观测性

增加低基数计数：无兼容 pair、绑定版本不可用、同版本重试、回执缺失、接单版本/实例失配、元数据不可用和协议不可用。

trace 中记录 logical ID、attempt ID、bound key、P/D 的 Worker ID 与实例、room、回执及失败阶段。版本、RID、room 不作为 Prometheus label。不要把请求文本或完整 prompt 作为版本诊断日志输出。

区分 `planned`、`dispatched`、`admitted`、`completed`，避免把成功选路或拿到回执展示成完成生成。性能数据按 HTTP/gRPC 和流式/非流式区分。

## 14. 代码改动落点

| 位置 | 内容 |
|---|---|
| `core/pd_pair_planner.rs`（新增） | key 半连接、候选准入、类型化选路失败 |
| `routers/pd_request_state.rs`（建议新增） | retry 外层状态、attempt 身份与提交边界 |
| `config/types.rs`、配置转换及启动入口 | legacy/strict、规范模型 UID、参数校验 |
| `routers/http/pd_router.rs` | 共享 planner、约束/回执、双派发失败收敛 |
| `routers/grpc/pd_router.rs`、`pipeline.rs`、`context.rs` | 外层绑定传入每次 pipeline |
| `routers/grpc/common/stages/worker_selection.rs` | 复用 planner，保留 policy 解析方式 |
| `request_building`、`request_execution`、`dispatch_metadata` | attempt ID、双端约束、回执和固定版本 |
| `routers/grpc/client.rs` | 已就绪外部 client 的 metadata 适配 |
| `core/retry.rs`、路由错误处理 | 在不改变 legacy 的前提下接入类型化重试判定 |

相同 key 的纯比较与集合算法放在共享模块；HTTP/gRPC adapter 只处理协议差异，不分别复制一套兼容逻辑。

## 15. 测试计划

### 15.1 规划与属性测试

| 编号 | 输入 | 断言 |
|---|---|---|
| R1 | 唯一 P(v10)、唯一 D(v11) | 无计划、无 dispatch |
| R2 | P(v10)/P(v11)，只有 D(v11) | 只能得到 v11 pair，不受 P(v10) 缓存命中影响 |
| R3 | 两端都为 v10、模型 UID 不同 | 不配对 |
| R4 | 两个完整版本组 | 得到其中一个完整组，不出现混合 pair |
| R5 | 指定版本 v10 | 不选 v11；v10 不完整时明确失败 |
| R6 | 无元数据、default、过期、非 Ready、缺 guard | 全部不进入 strict 候选 |
| R7 | registry 混有 HTTP/gRPC、其他角色/模型 | 不跨连接、角色或模型域 |
| R8 | 各 policy 在合法候选上运行 | 索引与 key 正确，不破坏其他组提示 |
| R9 | 选完 P 后 D 状态改变 | 有限失败/同 key 重规划，不放宽约束 |
| R10 | 候选随机生成 | 每个成功计划两端 key 相同且满足请求约束 |

属性测试以简单笛卡尔积仅作为测试 oracle：若稳定候选中存在合法 pair，且测试 selector 是 total 的，半连接必须能找到一个；若不存在，不能生成计划。生产算法不构造笛卡尔积。

### 15.2 HTTP/gRPC mock 集成

记录真实 dispatch payload/metadata，而不只检查最终响应版本。覆盖首次失败后切换同版本实例、仅新版本剩余、元数据中途变化、两端不同实例约束、回执缺失、伪造客户端内部字段和 disabled target header。

每个 retry 必须有不同 RID/room，P、D 共用本次 room；logical ID 与 bound key 不变。单独校验 gRPC context 重新创建后绑定仍保留。

模拟 decode 立即拒绝而 P 等待握手，断言及时取消 P，不等到全局长超时才处理。模拟一端成功接纳、另一端失败，验证负载 guard 和 breaker 不重复记账。

测试在客户端响应头提交前可以按规则重试，提交后即使没有输出 token 也不建立新 attempt。测试 native、chat、HTTP completions 各自的入口转换；不能用 generate 的测试替代所有入口。

### 15.3 验证边界

本 PR 的 mock Worker 模拟 `drain_v1` 合约，只用于确认 Gateway 行为。GPU 原地更新竞态、实际 KV provenance、断连后引擎终止和真实 gRPC servicer 校验属于 PR 3 的必过验收，不能在此标记已通过。

## 16. 性能与合并标准

测量兼容集合构造与完整选路的 p50/p95/p99、CPU 分配、Worker 数和版本组数量增长曲线。稳态不发生元数据 RPC，不重复解析完整 checkpoint 信息。与 legacy 对照时同时报告 no-pair 比例和正确请求吞吐，不能用错误跨版本命中换性能。

合并要求是：共享 planner 和 retry 状态已有稳定回归；所有成功 mock 尝试满足固定 key；外部响应提交后无隐藏重试；无候选错误不会被通用 503 retry 放大；旧 Worker 不被伪装为 strict；legacy 测试无预期外回归。

建议提交组织为 planner 与属性测试、HTTP 接入、gRPC 状态/adapter 接入、失败分类与取消测试。仅在真实 gRPC 依赖到位时接通该传输，不能通过手改生成绑定制造兼容假象。

## 17. 源码依据

[^S1]: `sgl-model-gateway/src/routers/http/pd_router.rs:978–1058`，HTTP 独立 P/D 选择。  
[^S2]: `sgl-model-gateway/src/routers/grpc/common/stages/worker_selection.rs:194–275`，gRPC 候选及策略调用。  
[^S3]: `sgl-model-gateway/src/routers/http/pd_router.rs:390–500`，retry 与 breaker outcome。  
[^S4]: `sgl-model-gateway/src/routers/grpc/pd_router.rs:76–192`，generate/chat 的 pipeline retry。  
[^S5]: `sgl-model-gateway/src/routers/grpc/common/stages/dispatch_metadata.rs:48–71`，从 Worker label 读取响应版本。  
[^S6]: `sgl-model-gateway/src/routers/grpc/pd_router.rs:218–244`，实际 RouterTrait 入口。  
[^S7]: `sgl-model-gateway/src/routers/grpc/pipeline.rs:162–204`，P/D pipeline stage 顺序。  
[^S8]: `sgl-model-gateway/src/routers/grpc/client.rs:129–148`，现有 Generate wrapper。  
[^S9]: `sgl-model-gateway/src/routers/grpc/context.rs:34–116`，每次请求的 context/state。  
[^S10]: `sgl-model-gateway/src/routers/http/pd_router.rs:652–811`，双派发及提前 decode 结果处理。  
[^S11]: `sgl-model-gateway/src/routers/grpc/common/stages/request_execution.rs:218–283`，双端 Generate 和 outcome 记录。

[^S12]: `python/sglang/srt/managers/scheduler.py:5426–5465`，abort 的 RID 前缀匹配。
