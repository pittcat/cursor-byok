# Cursor 3.27.22 协议对齐：当前状态与遗留问题

日期：2026-10-28  ·  关联：261028-2218-handoff.md（初诊）

## 已修复并部署（当前 main）

`server/src/api/cursor/bidi.rs` 容错解码：

- `decode()`：prost 解码 `AgentClientMessage` 失败时不再返回 400，降级为字段级打捞。
- `fields()`/`read_varint()`：手写 wire walker，无 schema 依赖。
- `salvage_run_request()`：从 run_request 恢复 `model_details`（选路）、`conversation_state`（能解则解，解不了容忍）、`action`（先试 `ConversationAction`，失败按 `{varint, string}` 启发式合成 `UserMessageAction`，mode=Agent）。
- 非 run_request 消息解码失败 → 返回 `message: None` 空消息，跟随 seqno0 已建立的 stream 路由（上游转发用原始字节，不受解码影响）。
- 回归测试：`bidi::tests::salvages_cursor_327_run_request`，fixture 为真实抓包 `server/tests/fixtures/bidiappend-3.27.bin`。

效果：原生模型（上游转发）与 BYOK 模型（本地管线）单轮对话均恢复。

## 遗留问题（按影响排序）

### 1. BYOK 多轮上下文缺失（最大缺口）

3.27.22 把 `conversation_state`（field 1）从内嵌 `ConversationStateStructure` 改为 **36 字节会话引用**（ASCII UUID，实测为 agentSessionId/anchor）。真实状态经加密 blob 通道交付：

- `agent.v1.AgentService` 上的 `PutAgentHostSessionBlobs` / `GetAgentHostSessionBlobs` RPC
- `GetLatestAgentConversationState`、`latestRootBlobId`、`conversation_state_blob_id`（`UserMessage` field 10、turn references）
- 加密：`blobEncryptionKey`、`x-blob-encryption-key`（key 派生方式未确认）
- turn/消息采用懒加载引用（`lazy turn references`）

BYOK 当前只能容忍引用、拿不到状态 → 多轮对话等于每轮失忆。

### 2. BYOK 本地 stream 后续消息被丢弃

`ExecClientMessage`（field 2）、`ExecClientControlMessage`（5）、`InteractionResponse`（6）、`ConversationAction`（4）、`kv_client_message`（3）oneof 分支也重排了字段（prost 报 `*.id: invalid wire type`）。打捞返回空消息 → 对上游转发无影响，但 BYOK 本地管线的工具审批、exec 结果、KV blob 同步进不来 → 需要工具调用/审批的 BYOK run 会卡住或缺结果。

### 3. 字段映射是实测推断，非官方 schema

wire 实测（3.27.22 BidiAppend seqno0 run_request）：

| wire field | 内容 | 旧 schema |
|---|---|---|
| 1 | 36B 会话引用 UUID | conversation_state |
| 3 | ModelDetails{model_id} | model_details ✓ |
| 4 | {1:varint, 2:text} = 用户 action | mcp_tools ✗ |
| 5 | "agent"（harness?） | conversation_id ✗ |
| 7 ×60 | 模型目录 ModelDetails | skill_options ✗ |
| 11 | {64hex, "stable", 模型列表} gateway credential | subagent_type_name ✗ |
| 16 | "unspecified" | conversation_group_id ✗ |

旧 proto 仅部分字段号仍吻合；bundle 中 `AgentRunRequest` 旧 descriptor 与实际 wire 不一致（真实写者类未定位，webpack 多份重复 proto 类）。

## 立项路线图

```
P0' 协议 watchdog（~2 天）——一次投入、每次升级复用
    ├─ bundle descriptor 提取器：main.js 的 static $() 字符串 → proto 字段映射
    ├─ 与 protocols/cursor/agent_v1.proto 自动 diff，标红重编号/变类型
    └─ 运行期自诊断：salvage 触发时记录结构化 field/wire 报告（免抓包）
P1  全 oneof 打捞（~2 天，依赖 P0'）
    └─ exec/interaction/conversation_action/kv 各分支按真实映射打捞，
       BYOK 本地管线恢复工具审批/exec 结果/KV 同步
P2  session-blob 状态通道（~1 周）
    ├─ 拦截 Put/GetAgentHostSessionBlobs → 本地 blob store
    ├─ x-blob-encryption-key 解密（key 来源未确认，最坏要再抓一轮）
    └─ latestRootBlobId 锚点 → 状态图重组 → 重建 ConversationStateStructure
P3  WebSocket /agent/v1/run（中工程）
    └─ 3.27.22 首选 WS，BidiAppend 为 ~60s 回落；消除每轮 60s 等待
```

## 运维注意

- 每次 Cursor 升级后：发一条消息 → 日志出现 `salvaging fields` warning 即说明协议又变了 → 跑 P0' diff 工具。
- 备选零成本路径：锁定 Cursor 版本关闭自动更新。
- 构建：rsproxy 已配在 `~/.cargo/config.toml`（国内拉依赖必须）；`cd apps/desktop && npm install && npm run tauri:build`。
- 部署产物：`target/release/bundle/macos/Cursor BYOK.app` → 替换 /Applications 并 `open`；旧版备份 `/tmp/Cursor BYOK.app-1.0.1-backup`。
- 抓包资料：`/tmp/byok-capture/raw-0{42,47,50,59}-api2.cursor.sh.bin`（dechunk+gunzip 后 BidiAppend body；`data` 为 hex 的 AgentClientMessage）。注意内含敏感会话数据，勿提交仓库。
- BYOK model_configs（sqlite `~/.cursor-byok-v3/cursor-byok.db`）：`ca6ef05c4f6688be`=MiniMax-M3.1-Flash（BYOK）、`3b48a34a6daaca67`=Kimi K3。

## 关键文件

- `server/src/api/cursor/bidi.rs` — decode/salvage（本次改动）
- `server/src/api/cursor/handlers.rs` — `bidi_handler` 选路（model_id→本地/上游）
- `server/src/cursor/compile/run.rs` — `prepare`/`action`/`mode_from_proto`（消费 decoded 消息）
- `server/src/cursor/conversation/runtime.rs` — `request.conversation_state` 消费点
- `protocols/cursor/agent_v1.proto` — 仓库 proto（字段号已滞后于 3.27.22 wire）
- `/Applications/Cursor.app/.../cursor-agent-host/dist/main.js` — 逆向源（`static $()` descriptor 字符串）
