# 2026-10-24 剩余性能差距定位

本轮继续检查[长历史优化](../history-20261024/README.md)后的波动，以及 rpi 与 Pi-Bolt 的剩余差距。证据指向整段历史复制、细粒度持久化与线程调度；RPC 字节更多，但抑制进度的实验不支持将其认定为默认场景的首要原因。

## 原来较大输出组的退化能否复现

相同 release 二进制、26,624 字节工具输出，依次执行 20 read、20 echo、10 次后续提示。将每组 3 次增加到 11 次，预热 2 次，52 个新进程交错运行，44 个正式样本全部校验通过，无删样。这里的“前/后”分别是 `e4baebb` 与上一轮未提交快照，尚不包含本轮工具副本修复。

| 持久化 | 版本 | 20 read ms | 20 echo ms | 后续 10 次提示 ms |
|---|---|---:|---:|---:|
| on | 前 | 52.98 | 108.22 | 88.50 |
| on | 后 | 53.17 | 112.33 | 71.37 |
| off | 前 | 39.20 | 90.61 | 88.96 |
| off | 后 | 39.72 | 89.18 | 65.32 |

原先关闭持久化时 read 约 +9.9%、echo 约 +4.5%，本次变为 +1.3%、−1.6%；开启时 echo 本次约 +3.8%，原先约 −3.5%。不能以原来的三个样本断定某项改动引入了固定 10% 的退化，也不能把所有小幅差异都宣布为随机噪声。共享机器、样本范围和完整顺序均保留；未证明某一处修改是这些变化的唯一原因。[原始复测](results/large-repeat.json)。

## 确认了一处无用的深拷贝

`execute_tool_calls_parallel` 原来为每个可执行工具任务复制完整 `AgentContext` 和 assistant 消息，再把副本交给 `finalize_executed_tool_call`。后者只有注册 `after_tool_call` 时才读取这两个参数。默认未注册回调时，整条历史仍照常复制；只有一个工具也可能进入可并行路径。

同一诊断二进制提供四个控制，逐组交错运行，每组预热 2 次、测量 7 次：

- `rpi-control`：保持原算法与完整 RPC。
- `rpi-nocopy`：仅在没有 after 回调时跳过工具任务的历史和 assistant 副本，保留完整 RPC。
- `rpi-quiet`：仅抑制 RPC 进度事件，保留回复、最终 agent_end 与 agent_settled。**刻意改变输出，仅用于诊断，不能部署或当作产品提速。**
- `rpi-both`：同时启用上述两项诊断。

208 字节工具输出，100 read、100 echo、40 次提示，72 个进程、56 个正式样本，单位 ms：

| 持久化 | 控制 | 100 read | 100 echo | 后续 40 次提示 |
|---|---|---:|---:|---:|
| on | 原算法 | 159.34 | 197.76 | 115.00 |
| on | 仅跳过无用副本 | 154.27 | 183.43 | 113.08 |
| on | 仅抑制 RPC 进度 | 168.76 | 203.89 | 113.35 |
| on | 同时启用 | 166.70 | 196.93 | 113.78 |
| off | 原算法 | 92.40 | 128.94 | 82.69 |
| off | 仅跳过无用副本 | 88.77 | 118.37 | 82.77 |
| off | 仅抑制 RPC 进度 | 88.36 | 124.07 | 80.45 |
| off | 同时启用 | 84.56 | 112.59 | 78.32 |

仅去副本时 echo 耗时降低约 7.2% / 8.2%，支持这处复制有实际成本。40 次后续提示不执行工具，因此该控制不应消除其每请求历史处理成本。

另测较大输出、关闭持久化，每组仍测 7 次：仅去副本的 read 40.03→39.75 ms，echo 90.41→89.73 ms，收益不到 1%；仅抑制进度则为 36.79 / 87.26 ms。这说明收益依赖负载，不能用其中一组推断所有工具场景。原始数据：[小输出控制](results/controls-small.json)、[大输出控制](results/controls-large.json)。

生产修复只加入“无 after 回调时不创建副本”的判断；注册回调时保留原上下文、消息和覆盖结果。任务仍通过原来的 spawn 执行，完成顺序、结果源顺序、取消和后台生命周期保持原实现。没有加入诊断环境变量，没有抑制 RPC，也没有删除恢复记录。见 [fix.patch](fix.patch)。

## 调用栈中的剩余工作

对上一轮代码构建带符号 release，500 read + 500 echo + 100 次后续提示，固定 208 字节输出；分别对持久化 on/off 采样。此处是加长历史的定位实验，不是正式速度排名。

- off：2,684 个 cpu-clock 样本，无丢样。约 20.38% 样本调用链经过 `Vec<AgentMessage>::clone`；`malloc`、`cfree` 的 self 占比分别约 8.05% / 3.06%。消息复制仍然是热点。
- on：3,671 个样本，无丢样。`_raw_spin_unlock_irqrestore` self 约 18.06%，调用栈包含 futex 唤醒、Tokio blocking 调度和追加写入。
- 每次模型请求仍在 `stream_assistant_response` 复制历史，再走 owned converter。Anthropic 仍需构建 wire messages，并完整编码最终 JSON。该诊断中 HTTP 请求累计字节分别约 62.75 MB（read）与 205.11 MB（echo）；不能把这类遍历和编码当成已消除的工作。
- `AgentHarness::build_stream_fn` 使用同步 `StreamFn` 包裹异步 Provider，每次经 `block_in_place` / `Handle::block_on`；文件追加和外部工具执行也有 blocking 桥接。源码和调用栈确认这些路径存在，但没有把 futex 总数唯一归因于某一个桥接函数。

inclusive 比例相互重叠，不能相加；部分调用栈地址未解析。采样比例不能直接解释成默认 100 次负载的可提速百分比。详见 [off self](diagnostics/cpu-off-self.txt)、[off 调用链](diagnostics/cpu-off-children.txt)、[on self](diagnostics/cpu-on-self.txt)、[on 调用链](diagnostics/cpu-on-children.txt)及完整 stacks 文件。

## 持久化记录与系统调用差异

同样的 100 read + 100 echo + 40 次提示，完整结算后的 JSONL 累计行数：

| 程序 | read 后 | echo 后 | 提示后 |
|---|---:|---:|---:|
| rpi | 1,011 | 2,021 | 2,421 |
| Pi-Bolt | 206 | 408 | 488 |

rpi 的记录包含 assistant_frame、step_attempt、tool_started、operation 开始/结束和消息条目。两种格式与恢复设计不同，不能把行数比当成纯存储引擎快慢，也不能据此断言 Pi-Bolt 不支持恢复。rpi 的现有恢复边界已由测试覆盖，本轮保留这些记录。

同一负载各做一次 strace 诊断：

| 系统调用 | rpi | Pi-Bolt |
|---|---:|---:|
| futex | 25,212 | 3,310 |
| write | 6,652 | 1,693 |
| statx | 2,622 | 825 |
| 全部调用 | 44,843 | 10,180 |

write 包含 stdout、会话等输出，不能全部算作 JSONL 写入。多线程 strace 会累计等待并显著干扰执行，不能把其秒数或“70% futex”当成真实 CPU 占比。这里使用调用次数和 CPU 调用栈判断调度工作较多。[rpi 跟踪](diagnostics/syscalls-rpi-on.strace.txt)、[Pi-Bolt 跟踪](diagnostics/syscalls-bolt-on.strace.txt)。

## 为什么不能把差距都归给 RPC 或扩展 ABI

上一轮正式样本中，rpi 的 read/echo RPC 输出约 771,615 / 953,315 字节，Pi-Bolt 为 551,743 / 626,234 字节，rpi 约多 40% / 52%。但本轮“仅抑制进度”在默认持久化下反而变慢，没有稳定支持“RPC 是首要瓶颈”。输出处理可能改变调度与重叠，不能用两组中位数简单相减，所得差额也不能当成各组件独立预算。

扩展工具存在参数 JSON、FFI 和 blocking 驱动；本轮没有将 ABI 成本单独隔离。echo 排在 read 之后，带有更长历史和不同输入，不能用 echo−read 的差额直接推断 ABI 成本。

## 生产修复的正式对照

本节“修复前”是上一轮长历史优化后的快照，“修复后”仅再加入本轮工具副本修复，均使用默认 release 配置。每组预热 2 次、测量 7 次，交错运行 54 个新进程、42 个正式样本，全部通过。单位 ms，七次中位数：

| 持久化 | 程序 | 100 read | 100 echo | 后续 40 次提示 |
|---|---|---:|---:|---:|
| on | rpi 修复前 | 156.26 | 191.61 | 106.95 |
| on | rpi 修复后 | 148.58 | 182.01 | 104.93 |
| on | Pi-Bolt | 155.24 | 142.49 | 73.64 |
| off | rpi 修复前 | 89.51 | 122.27 | 79.13 |
| off | rpi 修复后 | 87.40 | 115.44 | 81.23 |
| off | Pi-Bolt | 150.64 | 137.44 | 75.37 |

echo 的耗时下降约 5.0% / 5.6%，与同二进制控制的方向一致，幅度不同。短提示不经过工具任务复制路径，off 的中位数增加约 2.7%；不能将本轮写成所有阶段都改善。

修复后，默认持久化 read 在这组数据中已略快于 Pi-Bolt，echo 和长历史提示仍较慢；关闭持久化后，read 和 echo 快于 Pi-Bolt，短提示仍略慢。rpi 的 on/off read 中位数相差约 61 ms，而 Pi-Bolt 约 5 ms；该差值同时包含存储和恢复路径的变化，不能全部归到单个 write，但支持继续优先检查持久化提交与调度。

[正式数据](results/fixed-small.json)保留全部样本、范围、请求字节、RPC 事件和记录数量。修复前后 read/echo 请求字节数、完整协议事件数量及持久化记录类型/数量一致，没有以少发消息或少写恢复记录换取这项收益。

较大输出也完成七次正式对照（每组预热两次，36 个进程、28 个正式样本），单位 ms：

| 持久化 | rpi | 20 read | 20 echo | 后续 10 次提示 |
|---|---|---:|---:|---:|
| on | 修复前 | 54.80 | 110.30 | 70.41 |
| on | 修复后 | 52.59 | 107.70 | 74.37 |
| off | 修复前 | 40.23 | 88.65 | 65.47 |
| off | 修复后 | 38.83 | 87.27 | 66.97 |

工具阶段中位数下降，提示阶段分别增加约 5.6% / 2.3%。修复没有改动后续提示的算法，但前序工具处理、分配器和调度仍可能影响同进程后续阶段；本轮没有单独隔离这些影响，不能确定提示差异的唯一来源。[大输出原始数据](results/fixed-large.json)保留全部样本。这项修复是一个范围明确的工具路径改进，并未证明消除了所有负载的退化。

## 验证与复现

本轮新增包含 64 KiB 历史的双工具回归，分别运行有/无 after 回调两种情况，检查回调看到完整历史、系统提示、工具和 assistant，并保留结果覆盖与源顺序。

- Windows Agent 完整测试 37 通过、0 失败、1 个既有忽略；最终回调测试重新运行 5 通过。
- Linux Release 的 Agent 单元/事件/回调/取消/顺序/晚更新，以及 Harness 单元/端到端/恢复/会话状态共 303 通过、0 失败；最终回调用例重新运行 5 通过。
- Agent all-target Clippy 完成，既有警告保留；本轮改动行无新增诊断。上一轮工作区 2,106 项测试记录见[前一轮报告](../history-20261024/README.md)，没有将它冒充本次完整工作区重跑。
- Windows Release 已安装至 `C:/Users/bigfish/.cargo/bin/rpi.exe`，SHA-256 与本次构建一致，版本显示仍为 0.8.4。重试、持久化/续接历史、RPC Unicode/状态冒烟检查通过，见 [安装记录](installed-windows.json)与[冒烟记录](installed-smoke.txt)。原运行进程继续使用已保留的旧映像。
- 源码位于 `0.9.0-test` 的 `e4baebb` 基础上的未提交工作区。[前一轮快照](source-before.json)、[生产修复快照](source-fixed.json)、[诊断快照](source-controls.json)和[二进制/采样元信息](provenance.json)分别归档。
- 诊断二进制使用 `CARGO_PROFILE_RELEASE_DEBUG=1`、`CARGO_PROFILE_RELEASE_STRIP=none`。正式构建使用仓库 release 配置，没有这些覆盖。
- 沿用固定本地 Anthropic SSE、隔离配置与 HOME、CPU 亲和性和完整 settlement；无真实模型调用。共享开发机，正式测量期间未并行编译。
- [summary.csv](summary.csv)保留全部组的中位数、范围、CPU、RSS 和 agent_end 边界。[validation.json](validation.json)列出实验分类和样本数；所有预热与正式轮次均保留。

基准驱动沿用[正式驱动](../history-20261024/sources/compare.py)与[大输出驱动](../history-20261024/sources/large-compare.py)，通过 `--baseline ../runtime-20261024/sources/archived-run.py` 显式指定固定夹具实现。诊断控制驱动、各环境变量 wrapper 与 [controls.patch](controls.patch)放在 sources/；控制 flags 仅存在于 `/root/rpi-cause-20261024/source-controls`，未加入产品源码。

执行 `python3 sources/verify-results.py` 可离线复核驱动哈希、样本数、工具校验、Rust 请求字节/记录/协议事件数量，并重算 summary.csv。

后续优化优先级：保持原恢复边界，减少追加提交的阻塞任务往返；让没有自定义转换的请求路径借用历史或增量构建 wire 表示；再单独评估同步/异步桥接。最终 HTTP JSON、必要恢复状态和客户端协议仍需保留。
