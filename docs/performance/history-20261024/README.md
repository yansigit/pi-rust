# 2026-10-24 长历史处理优化

本轮移除已有所有权的历史复制、关闭压缩时的多余分支读取，以及工具历史修复的正常路径重建。默认持久化下，长历史后的 40 次短提示从 139.21 ms 降到 109.02 ms（−21.7%）；不持久化时从 113.04 ms 降到 83.50 ms（−26.1%）。工具链路有升有降，仍不能宣称全面领先 Pi-Bolt，未改写或发布博客。

## 实现与语义

1. Agent loop 直接接收并使用调用方移入的上下文；Harness 将每次尝试的历史移交给 loop，重试时仍从已保存分支重建。系统提示、工具配置和重试入口保持原行为。
2. 关闭自动压缩时，跳过仅服务于压缩判断的首次全分支读取；开启压缩时仍保留压缩前后读取。未缓存有状态的 projector/transform，也未改变其调用语义。
3. 会话状态从最近条目向前查找，每个字段只取最新值；保留压缩前模型、thinking 和工具选择信息，以及空工具列表的语义。
4. JSONL 与内存存储直接返回 reducer 已拥有的追加结果，减少追加前后复制；存储内部仍持有独立数据，修改返回值不会修改已保存记录。JSONL 仍在写入并 flush 成功后提交内存状态。
5. 工具调用与结果完整匹配时直接复用消息列表，借用调用 ID；缺失、重复、未匹配和交错情形走原修复算法。重复 ID 按出现次数匹配，没有改成去重集合。

保留全部模型历史、恢复帧、运行状态和协议字段。公开接口、取消与恢复边界没有变更。主循环 owned converter 与最终 JSON 编码仍有必要的数据构建，本轮不代表端到端零拷贝。

具体改动见 [changes.patch](changes.patch)，以 `e4baebb6cddbebe90dcbf840aa4afeb46a309107` 为基础。

## 测试方法

- 优化前：`0.9.0-test` 的 `e4baebb`，已包含[上一轮优化](../runtime-20261024/README.md)。优化后：相同分支上的未提交源码快照，程序版本号仍为 0.8.4。
- 精确输入见 [source-before.json](source-before.json)、[source-after.json](source-after.json)。优化前使用上一轮实际测量字节清单，其生产源码与 `e4baebb` 按 CRLF/LF 归一化后相同；没有将 Git archive 的规范化字节冒充原始构建输入。归档前核验了优化后清单的全部 618 个文件。
- Linux x86-64，WSL2 6.6.87.2、glibc 2.39、Ryzen 9 7950X。Agent 固定 CPU 2–5，本地模型服务与驱动固定 CPU 0–1。共享开发机，未独占整机，正式测量期间未并行编译。
- Pi 1.3.0 / Node.js 24.22.0，Pi-Bolt 0.9.6 / JIT off。二进制、扩展和驱动哈希均在原始结果内；未更新比较对象。
- 确定性本地 Anthropic SSE，隔离 HOME 与配置，无真实模型网络请求。校验工具集合、每次工具结果、请求数量和最终 BENCH_DONE。
- 正式测试：固定 1 个扩展，4 个程序 × 持久化 on/off，每组预热 2 次、正式测量 7 次，固定种子交错运行。共 72 个新进程、56 个正式样本，全部通过，无删样。
- 同一会话依次执行 100 次 read、100 次 echo、40 次短提示；read 内容为 208 字节。表内统一等待完整结算，另存共同 agent_end 边界。
- CPU 由 `/proc` 采样，分辨率 10 ms；阶段结束静置后采样 RSS。中位数、最小/最大值、CPU、RSS 与 agent_end 时间见 [summary.csv](summary.csv)。

## 正式同机结果

单位 ms，七次中位数；on/off 表示会话持久化。

| 程序 | 持久化 | 100 read | 100 echo | 后续 40 次短提示 |
|---|---|---:|---:|---:|
| rpi 优化前 | on | 158.23 | 198.91 | 139.21 |
| rpi 优化后 | on | 160.38 | 195.84 | 109.02 |
| Pi | on | 306.70 | 320.46 | 136.00 |
| Pi-Bolt | on | 154.27 | 141.24 | 78.23 |
| rpi 优化前 | off | 97.57 | 136.94 | 113.04 |
| rpi 优化后 | off | 93.90 | 127.52 | 83.50 |
| Pi | off | 298.03 | 306.91 | 133.46 |
| Pi-Bolt | off | 150.53 | 138.60 | 74.04 |

持久化开启时，read 的中位数增加 1.4%，echo 降低 1.5%；关闭时分别降低 3.8% 和 6.9%。不能将本轮概括为所有工作负载都变快。read 开启时，优化前范围 155.07–168.84 ms，优化后 157.60–197.43 ms；共享机器上的波动保留在原始数据中，不以删除较慢样本消除差异。

短提示 CPU 中位数从 120→90 ms（on）、80→50 ms（off）；共同 agent_end 边界从 119.56→94.69 ms、112.89→83.34 ms。完整结算与 agent_end 的差值也会变化，不应把墙钟收益全部解释成某一次 clone 的成本。

## 较大工具输出诊断

另测 26,624 字节输出（1,024 行，低于 read 截断限制）：20 次 read、20 次 echo，再做 10 次短提示。仅比较 rpi 前后；每组预热 1 次、测量 3 次，共 16 个进程、12 个正式样本。该实验用于检查负载敏感性，不能替代七次正式排名。

| 版本 | 持久化 | 20 read ms | 20 echo ms | 后续 10 次短提示 ms |
|---|---|---:|---:|---:|
| 优化前 | on | 56.00 | 109.14 | 92.16 |
| 优化后 | on | 53.29 | 105.37 | 67.19 |
| 优化前 | off | 39.02 | 90.73 | 94.75 |
| 优化后 | off | 42.88 | 94.80 | 70.28 |

短提示分别下降 27.1% 和 25.8%。关闭持久化时 read/echo 分别增加 9.9%/4.5%；三次样本不能证明原因，也不能支持“大输出一定更快”。后续如继续优化工具吞吐，应对该路径单独采样，而非依据本次短提示收益推断。

## 正确性与验证

- Windows 完整工作区：2,106 通过、0 失败，9 个既有忽略；真实 Provider DLL 与插件集成测试运行。
- Linux Release：Agent 事件序列、Harness 单元与端到端、恢复进度、会话状态测试共 292 通过、0 失败。
- 新增指针所有权回归，证明历史与追加返回载荷复用分配，并验证修改返回值不会污染存储。
- 工具修复快速路径与原实现对照 4,681 种短序列，覆盖完整/缺失/重复调用、并行结果、用户和自定义消息交错。既有重试、压缩和崩溃恢复测试通过。
- 两次实验全部 Rust 样本的 read/echo 请求字节数前后一致，持久化记录类型/数量与工具阶段 RPC 事件数一致。正式持久化样本阶段结束的累计 JSONL 行数均为 1,011 / 2,021 / 2,421。校验汇总见 [validation.json](validation.json)。字节数一致并不单独证明每个 JSON 字节相同；工具与模型内容另外由驱动校验。
- workspace/all-target Clippy 完成，沿用既有 `clippy::not_unsafe_ptr_arg_deref` 豁免，改动行无诊断。`cargo fmt --all -- --check` 与 `git diff --check` 通过。
- Windows 本地 Release 已安装至 `C:/Users/bigfish/.cargo/bin/rpi.exe`，与本次构建 SHA-256 一致，见 [安装记录](installed-windows.json)。重试请求、持久化与续接历史、原生 RPC Unicode/状态冒烟检查通过，见 [本地冒烟结果](installed-smoke.txt)。运行中的旧进程保留原映像，新启动命令使用本次版本；版本显示仍为 0.8.4。

## 数据与复现

[正式数据](results/comparison.json)、[较大输出数据](results/large-output.json)保存预热、所有正式样本、工具校验、CPU/RSS、RPC、会话文件统计和每次短提示明细。[正式驱动](sources/compare.py)、[大输出驱动](sources/large-compare.py)保持测量时的原始字节。

执行 `python3 sources/verify-results.py` 可离线重算全部 36 行汇总，并复核样本、记录数量和驱动哈希。

沿用[原始基准驱动](../runtime-20261024/sources/archived-run.py)、[依赖准备](../runtime-20261024/sources/setup.py)和[扩展夹具构建](../runtime-20261024/sources/build-fixtures.py)。需要 Linux x86-64/AVX2 与至少六个可用逻辑 CPU；固定依赖位于 `/root/bench-rpi-pi-bolt-20261021`。准备两个对应的完整源码快照并分别构建 release，不能将源码清单当作任意二进制的构建证明。

本次优化后二进制 SHA-256 为 `b55dd7b9b7701eb7fb0167a0ddafe0bd7c35d5139db00a4bcc4e3bddbd98dffa`，优化前为 `d5ac5efbf8b6ca755ca683eecf4159093ab90030f952be5da2c7c6c729f2eac1`。Linux 实际构建命令：

```sh
cargo build --release --locked --offline --jobs 4 -p rpi-cli \
  --manifest-path /root/rpi-history-20261024/source/Cargo.toml \
  --target-dir /root/rpi-perf-20261024/target
```

从本目录复测，显式指定 baseline，避免依赖驱动中的原始绝对默认路径。输出文件必须尚不存在：

```sh
python3 sources/compare.py \
  --baseline ../runtime-20261024/sources/archived-run.py \
  --binary /root/rpi-perf-20261024/target/release/rpi \
  --before-binary /root/rpi-history-20261024/rpi-before \
  --source-manifest source-after.json --before-source-manifest source-before.json \
  --tools rpi,rpi-before,pi,pi-bolt --profiles 1 --persistence on,off \
  --runs 7 --warmup 2 --output /tmp/new-history-results.json

python3 sources/large-compare.py \
  --baseline ../runtime-20261024/sources/archived-run.py \
  --binary /root/rpi-perf-20261024/target/release/rpi \
  --before-binary /root/rpi-history-20261024/rpi-before \
  --source-manifest source-after.json --before-source-manifest source-before.json \
  --tools rpi,rpi-before --profiles 1 --persistence on,off \
  --turns 20 --prompts 10 --runs 3 --warmup 1 \
  --fixture-repeats 1024 --output /tmp/new-large-output-results.json
```
