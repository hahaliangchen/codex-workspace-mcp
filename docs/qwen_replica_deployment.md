# Qwen 双卡副本部署

服务器：`10.10.10.201`，A100 80GB × 8。2026-10-19 在原有 GPU 4、5 的
INT8 TP2 实例之外，为空闲的 GPU 6、7 增加相同配置的实例。GPU 0～3 的服务不调整。

| 地址 | 用途 | GPU / 容器 |
| --- | --- | --- |
| `http://10.10.10.201:12888/v1` | 项目统一入口 | `qwen38-router.service` |
| `http://10.10.10.201:12890/v1` | 原实例直连、回退入口 | 4、5 / `qwen38-int8-tp2` |
| `http://10.10.10.201:12891/v1` | 新实例直连 | 6、7 / `qwen38-int8-tp2-gpu67` |

对外模型名均为 `qwen3.8-27b`。两组分别运行一个 TP2 副本，每组使用两张卡，
保留原有 W8A8 RTN 权重、MTP 3、262144 上下文、prefix caching、64 并发槽位、
8192 batched tokens 和 0.85 显存利用率参数。两份 KV cache 相互独立。
增加副本扩大并行服务容量，不代表单条请求的解码速度翻倍。

## 分流与流式输出

宿主为执行与压缩请求发送 `x-agent-affinity`，其值是任务 ID 与窗口 scope 的
稳定散列，与请求序号、轮次和提示词长度无关。主窗口、任务子窗口、压缩窗口使用
各自的分流键；同一工作窗口持续命中同一副本，避免每轮在两个 KV cache 之间切换。

Nginx 使用一致性散列。没有分流头的客户端按请求 ID 分流。此方式不保证某一会话的
少量子任务恰好平均分配；不同工作窗口可并行分布在两个副本。连接失败时跳过失败
后端；不启用非幂等 POST 在发送后的强制重放，也不在已开始的 SSE 中途重放请求。
响应包含 `X-Qwen-Backend` 供诊断。

代理关闭响应缓冲和 gzip，保留 SSE 即时输出。上游读取采用空闲超时，持续输出的
长请求不会因为累计运行时间被代理切断。

## 运行文件

服务器目录：`/root/data/fmq/codex-tuning-20261019-replica2/`。

- `replica2-run.json` / `launch-replica2.sh`：新容器的完整复现命令；使用原实例同一镜像 ID 与只读权重挂载。
- `qwen-router.conf`：独立 Nginx 配置，不修改服务器原有网站配置。
- `router-access.jsonl`：请求路径、后端、耗时、分流散列及宿主请求 ID；不记录提示词或请求正文。
- `router-error.log`：代理错误。
- systemd 单元：`/etc/systemd/system/qwen38-router.service`。

新容器使用 `unless-stopped`，统一入口由 systemd 开机启动。

## 检查与回退

```bash
systemctl status qwen38-router.service
docker inspect --format '{{.State.Health.Status}}' qwen38-int8-tp2-gpu67
docker logs --tail 30 qwen38-int8-tp2-gpu67
curl http://127.0.0.1:12888/v1/models
curl http://127.0.0.1:12890/health
curl http://127.0.0.1:12891/health
```

项目提供商 `qwen-local` 的地址位于
`/home/goodtime/.dsh/profiles/web/cordis.patch.yml`，切换统一入口不会改变插件模型授权。
宿主在新一轮请求加载最新配置；已开始的轮次保持原来的提供商快照。

需要回退时将该提供商地址改回 `http://10.10.10.201:12890/v1`。停止统一入口或新副本
之前，先确认对应后端没有进行中的请求。原实例的容器、端口、权重和参数均保留。

## 本次验证

两份实例均健康；分别通过原生工具调用及携带工具结果的后续对话，SSE 收到完整
`[DONE]` 和 usage，关闭思考后无 reasoning 输出。同一分流键重复访问保持后端一致。
5 路短文本并发分到两个副本，全部成功结束；本次合计约 289 token/s，耗时约 8.3 秒。
原实例在验证前还有其他请求，这不是隔离条件下的性能对比，也不是整份 PPT 耗时预测。

验证脚本与逐次结果保存在服务器部署目录的 `qwen201-verify-pool.py` 和 `results.json`。
宿主 167 项 Rust 测试通过，其中包含主/子窗口实际请求的分流头隔离验证。
