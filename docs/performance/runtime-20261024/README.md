# 2026-10-24 Linux 运行时复测与优化

复测没有得到“rpi 全面遥遥领先”的结果，因此没有改写或发布博客。本目录保存同机对照、定位过程和已验证的代码优化；10 月 21 日的历史数据保持原样。

## 测试对象与方法

- rpi：`feature-0.9.0` 的未提交工作区快照，程序版本号仍为 0.8.4。基础 HEAD 为 `87c29e99439082f0a78c079b53ba91f7e09c5b39`；HEAD 不包含本轮改动，精确文件哈希见 [源码清单](source-final.json)。
- Pi 1.3.0 / Node.js 24.22.0；Pi-Bolt 0.9.6，JIT off。沿用原始固定二进制和扩展夹具，并记录哈希。
- 平台：`Linux-6.6.87.2-microsoft-standard-WSL2-x86_64-with-glibc2.39`；AMD Ryzen 9 7950X 16-Core Processor。
- Agent 固定 CPU [2, 3, 4, 5]，本地模型与驱动固定 CPU [0, 1]。共享开发机，未做整机独占或温控；正式最终对照期间未并行编译。
- 确定性本地 Anthropic SSE，固定 208 字节 read 内容，校验模型、工具集合、每次工具结果和最终 BENCH_DONE。隔离 HOME 和配置，不调用真实模型或使用用户凭据。
- 正式对照：3 个程序 × 0/1/10 个扩展，每组预热 2 次、测量 7 次。81 个新进程，63 个正式样本，固定种子交错顺序，全部校验通过，无删样。
- 顺序与旧测试一致：100 次 read；有扩展时再做 100 次 echo；最后做 40 次短提示。带扩展组的短提示因此有更长的前置历史，不能把组间差额直接当成扩展加载成本。
- 表格墙钟时间统一等待完整结算：现代协议等待 agent_settled，旧版 rpi 等待完成 response。同时保存 agent_end_ms，便于按共同的 agent_end 边界比较。初始 get_state 兼容新旧响应字段。
- CPU 来自 /proc，分辨率 10 ms；RSS 为阶段结束静置后的五次采样中位数。采样器、系统调用跟踪和线程数控制实验均不混入正式速度成绩。

## 最终同机对照

下表为七次中位数，耗时单位 ms，RSS 单位 MiB。`—` 表示该组没有 echo 阶段。各阶段 CPU、共同 agent_end 边界以及样本范围见 [summary.csv](summary.csv)。

| 扩展数 | 程序 | 就绪 | 就绪 RSS | 100 次 read | 100 次 echo | 40 次后续短提示 |
|---:|---|---:|---:|---:|---:|---:|
| 0 | rpi | 7.36 | 14.75 | 158.19 | — | 100.67 |
| 0 | pi | 188.48 | 108.02 | 313.78 | — | 111.16 |
| 0 | pi-bolt | 28.62 | 81.00 | 153.15 | — | 59.53 |
| 1 | rpi | 7.47 | 15.75 | 159.83 | 199.07 | 141.98 |
| 1 | pi | 206.39 | 111.21 | 306.47 | 320.04 | 135.55 |
| 1 | pi-bolt | 43.41 | 103.75 | 158.07 | 149.73 | 78.27 |
| 10 | rpi | 8.83 | 17.50 | 174.52 | 208.16 | 146.24 |
| 10 | pi | 207.51 | 110.95 | 321.94 | 332.15 | 139.77 |
| 10 | pi-bolt | 60.63 | 110.00 | 161.06 | 146.77 | 81.10 |

rpi 仍明显领先启动和初始 RSS；默认持久化下，read 已接近 Pi-Bolt，但 echo 与长历史短提示仍有差距，不能写成全面领先。

## 本轮改动前后

以下“优化前”是本轮开始时、已经包含此前性能改动的 rpi 0.8.4 工作区快照，并非官方 rpi 0.6.3。比较均为 1 个扩展、持久化开启、七次中位数；前后属于两个独立交错实验，绝对差值仍受共享机器状态影响。

| 工作负载 | 本轮优化前 ms | 本轮优化后 ms | 耗时变化 |
|---|---:|---:|---:|
| 100 次 read | 218.47 | 159.83 | -26.8% |
| 100 次 echo | 272.75 | 199.07 | -27.0% |
| 40 次后续短提示 | 204.69 | 141.98 | -30.6% |

实际保留了三项改动：

1. **文件追加合并为一次阻塞任务。** 在同一任务内完成实际文件标识检查、必要重开、write_all 和 flush，减少逐条恢复帧触发的线程池往返。复用不超过 64 KiB 的缓冲区。取消 await 后仍保留并等待已经启动的任务，防止下一次追加越过未完成写入。Unix 继续校验设备号/inode；Windows 等没有安全稳定标识接口的平台继续逐次重开当前路径。
2. **借用已经符合 Anthropic 转换规则的历史。** 检查同模型、完整工具结果、thinking 和图片条件后借用原切片。跨模型、异常消息、缺失工具结果、图片降级等情况使用原始完整转换。公开的 owned 转换 API 保持原行为。
3. **后台任务只携带请求参数。** 从借用的 Context 构建拥有自身数据的 Anthropic 请求参数，省去后台任务启动前的 Arc::new(ctx.clone())。凭据校验仍先于构建，失败仍通过流式 Error 返回。

接口没有减少模型历史、恢复帧、运行状态或 RPC 字段；没有通过关闭功能取得默认配置成绩。

## 定位证据

- 一次带符号的 perf 诊断取得 654 个 CPU 样本，无丢样；约 32.9% 样本落在 `_raw_spin_unlock_irqrestore`，调用栈包含 futex 唤醒与追加记录的阻塞线程派发。该诊断使用带符号构建，仅用于定位，比例不代表可以全部消除的工作量。
- 同样的 100 read + 100 echo + 40 短提示，strace 诊断的 futex 调用从 **43,388 降为 25,326（−41.6%）**；总系统调用从 66,005 降为 46,732。strace 会放大耗时，其多线程等待时间不能解释成 CPU 占比。
- read、echo、短提示结束时的 JSONL 累计行数在改动前后分别都是 **1,011 / 2,021 / 2,421**。校验文件写入可见性、原生替换、失败不提交状态和取消顺序，避免以丢记录换取速度。
- 另做 TOKIO_WORKER_THREADS=1 控制实验，未消除剩余差距；没有修改产品默认线程数。

## 补充控制实验

以下每组预热一次、测量三次，仅定位原因，不替代上面的七次正式结果。

### 关闭会话持久化，保留相同工具历史

| 程序 | 100 次 read ms | 100 次 echo ms | 40 次后续短提示 ms |
|---|---:|---:|---:|
| rpi | 94.76 | 127.05 | 108.89 |
| pi | 296.77 | 319.30 | 128.61 |
| pi-bolt | 155.08 | 140.50 | 78.23 |

关闭持久化同时更换了存储路径，并启用 rpi 的内存恢复帧；不能把差值全部算成某条 write 的成本。rpi 在这组 read 和 echo 上快于 Pi-Bolt，长历史短提示仍较慢。

### 新会话直接做 40 次短提示

| 程序 | 新会话 40 次 ms | 完成 100 read + 100 echo 后的 40 次 ms |
|---|---:|---:|
| rpi | 62.53 | 141.98 |
| pi | 115.26 | 135.55 |
| pi-bolt | 50.27 | 78.27 |

这里的“新会话”也会在 40 次提示中逐渐积累历史；只是没有前面的 200 次工具调用。rpi 的耗时随前置历史明显增加。该对照更换了前置历史和会话状态，尚不能把全部差值归给某一次 clone。主循环 owned converter 仍复制历史，每次请求仍完整编码 JSON，扩展仍有 ABI/JSON 边界；后续应继续分别测量这些路径，而不能宣称已实现端到端零拷贝。

## 验证

- Windows 工作区完整测试：2,101 通过、0 失败、9 个既有忽略。真实 Provider DLL 和插件夹具集成测试运行。
- Linux Release：rpi-ai 237、rpi-harness 198、rpi-tools 76，共 511 通过、0 失败。
- 新增历史借用与原转换的多条件、逐前缀对照；保留原生文件替换、写入失败、提交绑定等回归，新增取消等待顺序测试。
- workspace/all-target Clippy 完成，沿用现有 raw-pointer lint 豁免；Rust 格式与 diff 空白检查通过。

## 原始数据与复现

- [正式最终样本](results/final-comparison.json)、[优化前样本](results/before-optimization.json)。所有轮次、预热、版本、二进制/夹具/驱动哈希、工具校验、RPC 字节、阶段 RSS 和短提示明细均保留。
- [不持久化控制](results/final-no-session.json)、[新历史控制](results/final-fresh-context.json)。
- [仅改文件追加](results/append-optimization.json)、[再加规范化借用](results/normalization-optimization.json)、[单 worker 控制](results/worker-one-before.json)为三次测量的中间定位实验，不能冒充七次正式结论。
- [CPU 调用栈](diagnostics/cpu-before-callgraph.txt)、[改动前系统调用](diagnostics/profile-syscalls-before.strace.txt)、[改动后系统调用](diagnostics/profile-syscalls-after.strace.txt)。原始 perf.data 和 Linux 二进制保留在测试机 `/root/rpi-perf-20261024/`，未作为二进制文件提交到仓库。
- [对照驱动](sources/compare.py)复用[原始驱动](sources/archived-run.py)的模型服务、校验、亲和性与采样；适配新旧 get_state，并为所有程序记录两种结束边界。优化前驱动另存 [compare-before.py](sources/compare-before.py)。

归档脚本沿用原测试的固定根目录 `/root/bench-rpi-pi-bolt-20261021`。复测需要 Linux x86-64 / AVX2 和至少六个可用逻辑 CPU。先准备固定依赖和夹具：

```sh
python3 sources/setup.py
python3 sources/build-fixtures.py
```

在要测的完整源码快照中构建；本次命令为：

```sh
cargo build --release --locked -p rpi-cli \
  --manifest-path /root/rpi-perf-20261024/source/Cargo.toml \
  --target-dir /root/rpi-perf-20261024/target
```

正式最终对照的参数如下。输出路径必须未被占用，避免覆盖已有样本：

```sh
python3 sources/compare.py \
  --baseline sources/archived-run.py \
  --binary /root/rpi-perf-20261024/target/release/rpi \
  --source-manifest source-final.json \
  --tools rpi,pi,pi-bolt --profiles 0,1,10 --persistence on \
  --runs 7 --warmup 2 --output /tmp/new-runtime-results.json
```

更换源码时必须生成对应的新清单，不能沿用 source-final.json 冒充源码一致。归档二进制哈希证明测量对象，源码清单记录构建输入；驱动不会自动证明任意外部二进制与清单的构建关系。本次快照复制时和归档时均核对了全部文件哈希。没有重新下载“最新版本”、调整功能默认值、改写原始博客或发布线上内容。
