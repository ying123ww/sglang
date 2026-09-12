# gRPC PD weight-version selection bugfix

## 结论

`PolicyKV-PD` 章程描述的请求级快照一致性是合理的长期目标，但它包含选择、重试、
worker 接单、权重更新和 KV 传输五个不同问题。当前已复现的 bug 只位于 gRPC
Gateway 的 P/D worker 选择层，因此本次交付只关闭这个最小闭环，不等待 GPU 实验。

本次可以声明：当 gRPC worker metadata 提供 `weight_version` 时，Gateway 不会选择
已知版本不同的 P/D 组合，并且候选池中存在同版本组合时仍能找到该组合。

不能据此声明：一次请求在重试、worker 权重更新或 KV 传输期间始终绑定同一快照。

## 问题

修复前，gRPC PD selector 分别在所有可用 prefill 和 decode worker 中调用 policy。
因此只有 `P(v10)` 和 `D(v11)` 时会返回异版本组合；如果
`P={v10,v11}, D={v11}`，先独立选择再比较版本还可能错误拒绝本来存在的合法组合。

补丁前定向回归结果：

```text
running 2 tests
matching_pair_is_selected ... ok
mismatched_pair_is_rejected ... FAILED

left:  Some(("v10", "v11"))
right: None
```

## 修复

修改文件：

```text
sgl-model-gateway/src/routers/grpc/common/stages/worker_selection.rs
```

选择顺序现在是：

1. 保留原有 model、连接模式、role、健康状态过滤。
2. 删除没有任何同 `weight_version` decode 候选的 prefill worker。
3. 在剩余 prefill 候选中调用原有 policy。
4. 只保留与所选 prefill 同版本的 decode 候选，再调用原有 policy。

版本是 hard constraint，policy 仍负责在合法候选中做原有负载或缓存策略选择。
没有采用“先分别选择、版本不同再拒绝”的实现。

为了兼容没有发布版本 metadata 的旧部署，两个都缺少 `weight_version` 的 worker
仍视为同一个 legacy 组；只有一侧缺失时不会与已知版本配对。这不是完整 strict
snapshot 身份保证。

## 验证

运行：

```bash
bash .local/run-cpu-regression.sh
```

补丁后结果：

```text
running 3 tests
test_pd_weight_version_matching_pair_is_selected ... ok
test_pd_weight_version_mismatched_pair_is_rejected ... ok
test_pd_weight_version_matching_pair_is_found_in_mixed_pool ... ok

test result: ok. 3 passed; 0 failed
```

同时通过 `cargo fmt --check` 和 `git diff --check`。这些测试直接调用真实 selector，
不启动 server，也不需要 GPU。

## 提交

```text
897ce89 test(gateway): reproduce PD weight version mismatch
1eb0346 fix(gateway): keep PD workers on one weight version
```

第一项提交只保存补丁前会失败的回归；第二项提交包含 selector 修复及混合候选池回归。

## 本次有意跳过

- GPU 模型推理和真实 KV transfer：它们用于验证下游后果，不是证明 selector
  是否返回异版本 pair 的必要条件。
- Mooncake/NIXL 数据面：selector 修复与 transfer backend 无关。
- HTTP PD 路径：当前 HTTP discovery 没有等价的可信 `weight_version` metadata 链路，
  应作为独立补丁补 metadata 后再过滤，不能用 mock label 冒充完成。
- retry 绑定、worker-side admission、权重更新事务和 transfer identity：属于完整
  PolicyKV 契约，不能塞进这次小 bugfix 后宣称完成。

## 下一步优先级

如果实际故障流量走 gRPC，先用真实 worker registration 确认两侧 metadata 中确实带有
`weight_version`，再做一次 Gateway 请求即可。只有实际部署走 HTTP，或者目标升级为完整
请求级快照一致性时，才继续章程后续工作包。
