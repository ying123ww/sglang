# RFC / PR 1：发布元数据归一化与路由策略候选集契约

**建议 PR 标题：** `fix(gateway): normalize execution metadata and honor policy candidate subsets`

**状态：** Draft。本文定义待实现行为，不表示相关代码或测试已经完成。  
**基线：** 本次上传 `sglang-main (1).zip`，归档标识 `3ed56a341b57eb57c8e37e0cec353e67ebb35c3b`。  
**前置依赖：** 无系列内依赖；gRPC 元数据扩展涉及外部 client/servicer，按第 5 节处理。  
**后续依赖者：** PR 2 的兼容 P/D 规划器；PR 3 的受保护 Worker 发布。

## 1. 摘要

本 PR 完成两个基础改动：将 Worker 自报的模型发布信息归一化为独立于普通 labels 的只读快照；修正有状态路由策略对“本次允许使用的候选子集”的处理。

HTTP 服务端 `/model_info` 已返回 `weight_version`，但 Gateway 的 HTTP `ModelInfo` 没有接收该字段。当前 Bucket 可能对被过滤的 URL 先记账，再因无法映射回候选数组而返回 `None`。Cache-Aware 可能把仅仅被本次筛选排除的健康 Worker 当作失效节点删除。Consistent hashing 已有 URL membership 过滤，本 PR 保留其实现并补测试，而不是重新实现一次。[^S1][^S2][^S3][^S4][^S5]

本 PR 不改变 P/D 的配对算法，不绑定请求版本，不实现权重更新互斥。合并后可以宣称元数据读取和候选子集契约得到修复，不能宣称已经提供在线更新期间的执行级版本一致性。

## 2. 问题与动机

### 2.1 版本数据在消费端丢失

HTTP 的服务端发布信息和 Gateway 的发现类型不对称。字段读取缺失会让后续路由只能依赖普通 labels、默认值或推测。gRPC 的 `to_labels()` 则把序列化结果中的若干标量转换成 labels，不能据此认定嵌套结构化发布信息也已被完整处理。[^S1][^S2][^S6]

现有 `create_worker` 明确允许配置 labels 覆盖发现 labels。这是普通标签的既有行为，不能将其直接复用为执行一致性的可信状态来源。[^S7]

### 2.2 全局策略状态与请求候选集不同

一个模型池可能同时保存 A、B 两个 Worker 的缓存和负载提示，而某次请求只允许 B。策略必须在 B 内完成选择，不能选择 A 后报错，也不能因为 A 本次不可选就删除 A 的全局提示。

该契约不限于版本路由，也适用于角色、健康状态、熔断状态和其他请求级筛选。因此，应先修复策略自身的子集语义，再由 PR 2 引入版本过滤。

## 3. 范围与非目标

范围包括 HTTP 版本字段读取、结构化发布信息的可选解析、gRPC 元数据适配接口、元数据快照生命周期、Bucket 与 Cache-Aware 修复，以及已有策略的候选子集回归测试。

不包含 Worker 发布状态的生产者、权重更新流程、请求线协议的执行校验、版本流量比例控制、KV 传输格式兼容推导、原生 Rust runtime 的 gRPC 协议迁移，也不把 Gateway 的缓存提示树改造成引擎 KV 缓存。

所有新增发布信息都是可选的。旧 Worker 仍能按 legacy 路径注册和服务。缺失字段只意味着后续 strict 路由不能信任其执行身份，不意味着它对旧业务“不健康”。

## 4. 统一数据模型

以下类型和字段均为本系列新增设计。

```rust
struct ExecutionKey {
    model_uid: String,
    weight_version: String,
}

struct ExecutionInfo {
    schema_version: u32,               // 首版为 1
    key: ExecutionKey,
    instance_id: String,                // 一次进程生命期的唯一标识
    publication_seq: u64,               // 同一实例内的发布状态序号
    state: PublicationState,           // Ready / Draining / Updating / Unavailable
    version_guard: String,             // 首版识别 "drain_v1"
}

struct ObservedExecutionMetadata {
    reported_weight_version: Option<String>,
    execution_info: Option<ExecutionInfo>,
    status: MetadataStatus,             // Valid / Missing / Invalid / Unsupported / Unreachable
    observed_at: Instant,
    registration_generation: u64,
    refresh_seq: u64,
}
```

这是接口级伪代码，实际使用现有类型风格、`Arc` 和序列化约定。

`reported_weight_version` 保留服务端原始标签，供诊断和旧行为使用。当前服务端未指定启动版本时默认返回 `default`，这与 HTTP 字段缺失不是一回事。[^S13]只有完整、有效的 `execution_info` 才能作为 strict 的依据。不能从一个普通版本字符串自动构造 `version_guard`、`model_uid`、Ready 或实例身份。

### 4.1 字段语义

`model_uid` 表示一个固定模型语义域，包含该域的 Tokenizer、模板及其他会影响输入解释的固定配置。它由部署配置显式指定；模型路径、对外 alias 与规范身份不是同一个概念。

`weight_version` 是该语义域下不可变的发布标识，只进行精确字符串比较，不进行数值排序或语义版本排序。发布方保证一个 key 始终对应同一发布产物；Gateway 不逐请求读取或哈希权重。

`instance_id` 不参与 P/D 版本相等判断。同一 key 的不同实例可以互相替代。

`publication_seq` 在每次对外发布状态转换时递增，包括 `Ready -> Draining`、恢复旧 Ready、进入 Unavailable 和成功发布新版本。它不只在权重更新成功时递增。这样，迟到的旧 Ready 信息不会覆盖新的 Draining 信息。

### 4.2 校验规则

首版受保护的模型 UID 和版本采用 1–256 字节、无空白的可打印 ASCII 标识。比较时不进行 trim 或大小写转换。保留值 `default` 不作为 strict 发布版本；诊断用的 `reported_weight_version` 仍保留原始自由格式标签，不强行改写旧 Worker 的值。

结构化对象缺必需字段、出现错误类型、未知 schema 或未知状态时，记录 Invalid 或 Unsupported，不能部分补默认值。未知 `version_guard` 不影响 legacy 注册，但不能进入首版 strict 候选。

首版限制 `execution_info` 的序列化大小不超过 4 KiB。这是协议资源上限，不是实测性能结论。合法的前向兼容附加字段可忽略；已知字段发生冲突则不能忽略。

顶层 `weight_version` 与 `execution_info.key.weight_version` 同时存在且不一致时，判为发布元数据冲突。两个未知版本不构成兼容关系。

## 5. HTTP 与 gRPC 的元数据传播

### 5.1 HTTP

在 `discover_metadata.rs::ModelInfo` 中增加可选 `weight_version` 与 `execution_info`。保留当前 `/model_info -> /get_model_info` 的 404 回退，不为版本另发一个逐请求 RPC。[^S2]

`weight_version` 来自 `/model_info`，不从软件版本字段、启动配置或 `/v1/models` 的模型名推导。发现 model alias/path 的现有回退行为保持不变。

建议 Worker 返回的形状如下；PR 1 负责读取，PR 3 才负责真实生产该对象：

```json
{
  "model_path": "/models/qwen3-release-10",
  "served_model_name": "qwen3-demo",
  "weight_version": "v10",
  "execution_info": {
    "schema_version": 1,
    "key": {"model_uid": "qwen3-demo", "weight_version": "v10"},
    "instance_id": "8f9cd233-6735-4e23-aecd-e8f617352116",
    "publication_seq": 3,
    "state": "ready",
    "version_guard": "drain_v1"
  }
}
```

解析失败不能使整个旧 Worker 发现流程失败；应保留普通模型信息，并单独记录执行元数据不可用。

### 5.2 gRPC：以实际连接的协议为准

本次源码的 Gateway 使用外部 `smg-grpc-client = "=1.0.0"`，Python gRPC 入口委托外部 `smg_grpc_servicer`。仓库内另有 `sglang.runtime.v1` 的新协议，其 `GetModelInfoResponse` 包含 `json_info`。它们不能因为都叫 SGLang 就被当成同一套绑定。[^S6][^S8][^S9][^S10]

本系列面向 `sgl-model-gateway` 当前使用的 SGLang Scheduler client 路径。为避免把 unrelated runtime proto 改动当成有效修复，首版提出用 **gRPC response metadata** 传递新增结构化对象：

```text
GetModelInfo response metadata:
  sglang-execution-info-bin = UTF-8 JSON bytes of ExecutionInfo
```

该键为新增协议约定。client 必须保留 `tonic::Response` 的 metadata，再将对象归一化；servicer 必须从运行中的发布状态读取它。typed model-info 中实际存在的顶层版本字段仍按原路径读取；缺少时保留为 Missing。仅凭本地 wrapper 无法列出外部包所有字段，必须由实际依赖的协议测试确认。

外部 client 如果当前不暴露 response metadata，需要先提交其兼容扩展，并在 Gateway 固定到实际已发布、包含该扩展的依赖版本。本文不虚构一个新版本号。旧 client/servicer 缺少该能力时，结果为 legacy-only，不能用 labels 模拟能力。

新 runtime 的 `json_info` 只在明确选用了对应 client adapter 时解析，不增加“尝试任意 proto 类型直到成功”的探测。新 runtime 的 strict 支持不属于本系列首版。

### 5.3 依赖验收

HTTP 工作和策略修复不应被 gRPC 依赖阻塞。PR 1 可先合并两者及统一类型；gRPC 的结构化解析分支只有在真实依赖提供 metadata 读取接口后才接通。不能导入尚不存在的外部方法，使 PR 无法独立编译。

gRPC 就绪标准是：真实 `GetModelInfo` RPC 经过所依赖的 client 返回同一 `ExecutionInfo`，而不是仅对手写 JSON fixture 测试通过。没有该证据时，文档必须保留 legacy-only 状态。

## 6. 快照存储、刷新与来源边界

### 6.1 独立只读存储

在 Gateway core 中新增 `ExecutionMetadataStore`，以 Worker 注册身份和注册代次定位快照。`Worker::metadata()` 当前返回不可变引用；不应为了热更新版本字段，将普通 labels 改成在请求线程中任意写入的共享对象。

首次发现结果由注册 workflow 传给该 store。普通 labels 继续采用既有配置优先级；strict 读取的结构化状态不经过 `config.labels` 合并，也不允许普通 WorkerUpdate labels 覆盖。

“可信”指已认证的 Worker 管理连接和部署方履行发布契约，不是对恶意 Worker 的密码学证明。TLS、服务身份和管理接口授权仍沿用部署安全要求。

### 6.2 刷新机制

注册时采样一次。刷新器作为可启停组件加入；PR 1 默认不为 legacy 部署启动新的周期任务，PR 2 在启用 strict 时启动它。

初始建议参数为间隔 5 秒、随机抖动 ±20%、有效期 15 秒、单次 RPC 超时 2 秒、最多 16 个并发刷新。这些都是可配置初值，最终默认值需用测试和实际控制面负载确认，不承诺它们适合所有集群。

失配事件触发一次合并刷新，同一 Worker 的并发触发共享同一次 RPC。steady-state 请求只读取内存快照，不访问 `/model_info`。后台任务在 Worker 删除和 Gateway 退出时取消并清理。

### 6.3 迟到响应与同 URL 替换

每次刷新携带注册代次和请求序号。提交结果前验证：Worker 仍是同一次注册、响应没有被更新的采样结果超越；同一实例的 `publication_seq` 不回退。

相同实例、相同序号且对象内容相同，可延长有效期；相同序号内容冲突，标记 Invalid。实例 ID 改变时按实例替换处理，不对 UUID 排序，也不将旧实例的状态序号与新实例比较。

刷新失败保留最后值供诊断，但将 strict 可用性设为 false；失败响应不能覆盖之后已成功提交的新采样。元数据失败与 GPU/网络请求失败分开计数，不顺手打开 Worker 熔断器。

同 URL 实例或 key 变化时，向缓存策略发送失效通知。只做状态版本变更且 key/实例不变时，不必清理已有缓存提示。

## 7. 路由策略候选集契约

对传入 `workers` 和 `SelectWorkerInfo`，必须满足：

```text
select_worker(workers, info) = Some(i)  =>  i < workers.len()
最终选择不能使用不在 workers 中的 URL、旧下标或全局池成员。
workers 为空或无可用候选               =>  None
```

调用方提供同一模型路由域和同一角色的候选，集合可以包含不同版本。健康或熔断状态随后变化仍是竞态，最终 dispatch 前由调用方再检查；策略不能通过回退到全局池绕过集合。

选择过程中的估算状态只能更新最终选中的合法 URL。估算缓存插入和 Bucket 统计不是 GPU 已执行或请求成功的证明；实际负载 guard 和成功指标由 dispatch/完成路径负责。

该 PR 不将所有策略重构成事务式评分接口，也不要求一次选择在并发更新下绝对成功。它要求“不越界、不因合法候选被全局状态遮蔽而错误失败、不删除被排除的健康状态”。

## 8. Bucket 修改

先基于当前候选构造 URL 到下标的 membership map；只使用当前可用候选的估算负载计算 min/max 和不均衡阈值。新加入且尚无历史值的合法 Worker 按现有无历史默认语义处理，不能从候选中消失。

若处于不均衡模式，在当前合法候选中选择最小估算负载。若处于 Bucket 模式，保留合法的 boundary 命中；boundary 指向集合外 URL 时，使用当前合法集合内的最小估算负载回退，平局采用现有随机打破方式。

再次确认所选 URL 能映射到当前候选下标后，才调用 `post_process_request`。没有合法结果时不增加任何 URL 的请求统计。

全局 bucket 的其他 URL 不删除、不重置。调整周期、历史窗口和按 prompt 长度分桶算法不在本 PR 重写。

## 9. Cache-Aware 与 Tree 修改

### 9.1 对合法 tenant 查找，而不是先命中再删除

在本地 `policies/tree.rs` 增加带 membership predicate 的查找，例如：

```rust
prefix_match_with_counts_filtered(text, |tenant| eligible_urls.contains(tenant))
    -> Option<PrefixMatchResult>
```

保留旧 API 的签名及空树返回语义，作为无过滤包装，避免迁移所有既有调用；不能将新 helper 的 `Option` 返回类型直接泄漏为旧 API 的破坏性变更。沿匹配路径记录最深的、至少存在一个合法 tenant 的节点及对应匹配长度；最深全局节点没有合法 tenant 时，可以返回合法祖先的较短命中。不能拿被过滤 Worker 的最长匹配长度给另一个 Worker 计缓存收益。

`last_tenant` 只是加速提示。它不属于本次合法集合时，应查找节点内其他合法 tenant；不能因此删除它、清空该节点或将其判为健康故障。UTF-8 字符计数和部分边匹配保持现有语义。[^S11]

### 9.2 策略行为

Cache-Aware 使用上述结果计算合法命中率。没有合法高命中时，在合法、可用集合内按现有负载方式回退，随后仅更新选中 Worker 的提示和选择计数。

从候选数组中查不到某个 tenant，不再触发 `remove_tenant`，也不发送 mesh removal。删除依据来自显式 Worker 移除、实例/key 变化和既有的缓存清理流程。[^S4]

首版仍采用现有 pool/model 树，不按候选列表第一个 Worker 的版本挑树，因为 PR 2 的未绑定请求可能同时拥有两个合法版本组。

### 9.3 提示准确性边界

Gateway 树是缓存位置估计，不是实际 KV 所有权。失效通知与在途插入并发时，短时间内可能留下过时提示；它最多影响选路效率，不能突破候选过滤或 PR 3 的引擎版本校验。本文不承诺跨 Gateway mesh 的版本精确缓存索引；首版 strict 部署不依赖 mesh 发布信息。

## 10. 其他策略

Consistent hashing 保留已存在的 `healthy_url_to_idx` 和 ring membership predicate。测试覆盖全局环包含集合外节点、子集重排、仅一个合法节点、全部被过滤和 fallback。[^S5]

数值型 `X-SMG-Target-Worker` 的兼容性在 PR 2 定义：strict 拒绝该 header，legacy 语义不变。本 PR 不偷偷将其从下标改成 URL。

Random、Round Robin、Power of Two、Prefix Hash 和 Manual 至少经过相同的子集契约测试。Manual 的 sticky 映射不得越过当前集合；首版不承诺引擎有状态 session 在 Worker 更换后的恢复，也不因版本功能重写所有 sticky 策略。

## 11. 代码改动落点

| 现有位置或建议新增位置 | 负责内容 |
|---|---|
| `core/steps/worker/local/discover_metadata.rs` | HTTP 可选字段；发现结果与普通 labels 分离 |
| `core/steps/workflow_data.rs`、`create_worker.rs` | 传递结构化结果；不经过用户 labels 覆盖 |
| `routers/grpc/client.rs` | 实际 client 的 model-info/metadata 适配 |
| `core/execution_metadata.rs`（新增） | key、发布快照、校验及 store |
| `core/worker_manager.rs`、注册/删除 workflow | 刷新器生命周期、注册代次和清理 |
| `policies/bucket.rs` | 子集负载、边界回退、合法结果后记账 |
| `policies/cache_aware.rs`、`policies/tree.rs` | 带合法 tenant 约束的匹配与回退 |
| `policies/mod.rs`、已有策略测试 | 候选集契约文档和回归 |

新增文件名是建议，不是基线中已存在的文件。尽量在小组件内实现，不借本 PR 重写 Worker、注册系统或整个策略 registry。

## 12. 测试与验收

| 编号 | 场景 | 必须断言 |
|---|---|---|
| M1 | HTTP 已有 `weight_version` | 精确保留；字段缺失/null 才为 None，服务端默认 `default` 保留为原始字符串 |
| M2 | 404 旧端点回退 | 保持原行为，无新增逐请求请求 |
| M3 | 缺少或损坏 `execution_info` | legacy 注册成功；strict eligibility 为 false |
| M4 | 配置 labels 伪造 guard/key | 不能改变结构化快照 |
| M5 | 顶层版本与结构化版本冲突 | 不能作为受保护发布使用 |
| M6 | 同实例旧 Ready 晚于新 Draining 返回 | 新状态保留，不延长旧 Ready 的 TTL |
| M7 | 同 URL 删除重注册、实例重启 | 旧注册代次响应不能覆盖新记录 |
| M8 | 同序号同内容、同序号异内容 | 前者可刷新，后者 Invalid |
| M9 | gRPC 实际 client 元数据读取 | 与 HTTP 对象一致；缺能力不能伪造 |
| P1 | Bucket 全局最小负载是被排除 URL | 返回合法候选，且不增加集合外统计 |
| P2 | Bucket 边界命中被排除 URL | 子集内回退，不错误返回 None |
| P3 | Cache-Aware 最深命中被过滤 | 返回合法祖先正确长度或负载回退 |
| P4 | `last_tenant` 不合法、同节点另有合法 tenant | 正确选择，未删除原 tenant |
| P5 | 全局树无合法 tenant | 返回合法负载候选，不广播删除 |
| P6 | Consistent hashing 全局环与子集不同 | 所有返回值属于子集 |
| P7 | 所有策略空集、单元素、重排、熔断 | 无越界、无未授权回退 |

使用假的单调时钟和可控制的 RPC 完成顺序，避免依赖 sleep 验证刷新竞态。计数测试应读取 Bucket 实际状态，Tree 测试应验证 tenant 与 mesh 消息，不仅检查返回下标。

建议运行已有 Gateway routing/reliability 测试，以及新增 `execution_metadata` 和 policy 单测。示例命令在测试实现后执行：

```bash
cd sgl-model-gateway
cargo fmt --all -- --check
cargo test -p sgl-model-gateway --lib
cargo test -p sgl-model-gateway --tests
```

基线依赖环境及各测试的外部服务要求按项目现有 CI 配置准备。本次 RFC 编写没有运行以上测试，不填写通过数量。

## 13. 性能与兼容性

稳态元数据读取不增加网络往返。筛选 URL map 的成本与候选数线性相关。过滤树匹配的成本包括匹配路径与检查的 tenant 数，不能继续无条件宣传 O(1) tenant 查找。

基准覆盖 1/8/64/256 个 Worker、1/2/8 个版本组、0%/50%/95% 候选排除，以及 ASCII/中文 prefix。报告选择时延分位数、分配次数、树占用及刷新 RPC 数量，不预填收益。

除明确修复的子集错误外，legacy 默认策略、配置 labels 优先级和协议形状保持兼容。缺新元数据的 Worker 不受拒绝。新树 helper 保留旧接口，降低回滚成本。

## 14. 合并标准与提交组织

本 PR 的合并条件为：HTTP 字段读取可用；结构化状态不会被普通 labels 覆盖；Bucket 与 Cache-Aware 的稳定回归用例在修复前失败、修复后通过；已有策略和 legacy 注册无预期外回归；未就绪的 gRPC 能力明确报告不可用。

建议在一个 PR 内组织成三组可单独审阅的提交：元数据类型与 HTTP 读取；快照 store 与生命周期；策略修复和测试。gRPC 外部扩展按真实依赖情况单独联动，不把不存在的包版本写入 Cargo 或 Python 依赖。

该 PR 不应增加“strict 已支持”的对外文档。完整路由语义由 PR 2 建立，执行保护由 PR 3 激活。

## 15. 源码依据

以下均指本次上传快照。文件路径和行号是本文“现有行为”的依据；设计章节中新增字段不属于这些既有实现。

[^S1]: `python/sglang/srt/entrypoints/http_server.py:791–836`，`model_info`。  
[^S2]: `sgl-model-gateway/src/core/steps/worker/local/discover_metadata.rs:48–63, 280–395`，HTTP 模型发现类型与读取。  
[^S3]: `sgl-model-gateway/src/policies/bucket.rs:233–301`，全局 URL 选择与先记账后映射。  
[^S4]: `sgl-model-gateway/src/policies/cache_aware.rs:426–526`，匹配、候选映射和移除。  
[^S5]: `sgl-model-gateway/src/policies/consistent_hashing.rs:69–106`，URL membership 过滤。  
[^S6]: `sgl-model-gateway/src/routers/grpc/client.rs:162–208`，typed response 和 labels 转换。  
[^S7]: `sgl-model-gateway/src/core/steps/worker/local/create_worker.rs:70–81`，配置 labels 优先级。  
[^S8]: `sgl-model-gateway/Cargo.toml:114–118`，外部 gRPC client 依赖。  
[^S9]: `python/sglang/srt/entrypoints/grpc_server.py:157–176`，外部 servicer 入口。  
[^S10]: `proto/sglang/runtime/v1/sglang.proto:218–226`，不同代际的 runtime model-info envelope。  
[^S11]: `sgl-model-gateway/src/policies/tree.rs:531–627`，现有 prefix matching 与 cached tenant。  
[^S12]: `sgl-model-gateway/src/policies/mod.rs:42–106`，现有策略接口。

[^S13]: `python/sglang/srt/arg_groups/fields/serving.py:165–168`，已有 `weight_version` 的默认值。
