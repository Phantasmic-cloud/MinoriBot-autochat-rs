# autochat-rs

Rust 版 `services/autochat`。目标只有两个：和 Python 版逻辑/功能一模一样，关 py 开 rs（或反过来）能接着同一份配置和数据干活；运行时更省内存、本地计算更快一点。

## 开发节奏

- **Python 是 source of truth。** 功能先在 `services/autochat/` 落地并验证，再把同样改动同步到本目录。
- rs **不多一项、不少一项、不改语义**。不要在这边抢先加 poke/actions/记忆之类的新能力。
- 不要“优化”成另一套架构。内存里的东西继续只放内存（关掉就没）；磁盘继续用现有路径和字段。

同步时对照这三样：RPC 方法与参数、磁盘字段、`serve.py` 的 `chat()` 分支（意愿 / 记忆 / actions / sticker）。

## 运行时契约

工作目录必须是 MinoriBot 仓库根（和 py 一样：`services/autochat/utils.py` 会 `chdir` 到 `_ROOT`）。二进制启动后自己往上找 `config/chat/autochat.yaml`，找到就 `chdir`。也认环境变量 `MINORIBOT_ROOT`。

读同一份配置：`config/chat/autochat.yaml`（`Config("chat.autochat")`，点号路径，mtime 热加载）。

连同一套 RPC：JSON-RPC 2.0 over WebSocket，`params` 是数组，**第一个参数是 token**。方法名、参数顺序、返回形态跟 `src/chat/autochat.py` 里注册的一致。

| 方法 | 参数（token 之后） |
| --- | --- |
| `get_self_info` | `group_id` |
| `get_group_list` | （无） |
| `send_group_msg` | `group_id`, `message` |
| `poke_group_member` | `group_id`, `user_id` |
| `set_msg_emoji_like` | `group_id`, `message_id`, `emoji_id` |
| `get_group_history_msg` | `group_id`, `limit` |
| `query_llm` | `model`, `prompt`, `images`, `options` |
| `query_embedding` | `texts`, `model_name` |
| `get_new_msgs` | （无） |

超时对齐 py：`query_llm` 用 `options.timeout + 5`，`query_embedding` 固定 60s，其余 `rpc.default_timeout`。主循环 1s 拉一次新消息；每群一个 worker；队列空闲 3 小时退出。

## 磁盘（必须读写同一份）

根目录：`data/chat/autochat/`

- `db.json` — `FileDB`，键 `status_{group_id}`：`willingness` / `self_msg_ids` / `last_check_willing_time` / `last_reply_time`
- `memory_{group_id}.json` — `ums`、`sms`（`sms[].text` 可选，表情包用 `sticker` 存「情绪/场景」）
- `image_captions.json` — `file_unique → caption`
- `sticker_db.json` — 只读，主程序维护
- `stk_emb_db.json` — `{emb_model, embeddings}`，caption 向量缓存
- `memory_chromadb_{group_id}/` — 事件记忆。collection 名是拼写错误的 **`event_memroy`**，不要改。

`FileDB.set` 的语义：改完立刻写盘（tmp + replace）。JSON 里元组会变成二元数组，`ums` 要兼容旧字段 `text`。

## 只放内存（关掉就没，禁止落盘）

和 py 一样：

- 表情包倍率 `sticker_multipliers`
- 表情包向量缓存 `_sticker_cache`（mtime 变了再重建）
- 每群 poke 时间线 `group_pokes`（最多 10 条）
- 每群 `self_info` 缓存
- 每群消息队列 / worker
- RPC 连接与 in-flight 请求

主程序消息池按 RPC 连接建、断开即清空。切换进程最多丢未被 `get_new_msgs` 拉走的那一两秒，群历史在 bot 侧。

## 必须咬死的行为

- **actions** 脏解析：字符串当 text；一个 object 可含多个 key，按 key 顺序展开；`poke` 标量或数组；`sticker` 要有 emotion 或 scene；`react` 为 `[msgid, emoji]`（数字 id 或非 ASCII 首字符码点）。上限 5 / text 3 / react 3。
- 文本里 `[@qq]`、`[reply=id]` 的剥离和 CQ 拼接、先 at 后 reply、只在 recent 里出现才加 CQ，然后 `truncate`（ASCII 宽 1，其它宽 2）。
- 意愿值：时间衰减、每条消息、@ 与回复可叠加但各自最多一次、关键字、群倍率；回复概率 `min(max(w,0),1)`；发出后再乘 `decay_after_send` 再减 `decrease_after_send`。
- 自己的消息 / 自己戳人：不触发。别人戳别人：只入时间线。明文以 `/` 开头：不触发。`msg.time <= last_reply_time`：丢掉（思考期间入队的消息）。
- 事件记忆查询距离用 Chroma 默认 **L2 平方**（不是 cosine）。短记忆再加 `hours * em_time_decay_per_hour`。混合多向量查询，按 `adjusted_distance` 去重取最小。
- 表情包检索才用 **cosine**；emotion 先过阈值，再在候选里用 full 分 * 倍率。
- prompt 用 Python `str.format` 规则：`{name}` 替换，`{{` / `}}` 转义。persona 先找群号键（int 或 str），否则 `default`。
- 日志：`[YYYY-MM-DD HH:MM:SS][LEVEL] ...`，级别集合 `DEBUG/INFO/WARNING/ERROR`。

## 事件记忆存储

py 用 `chromadb.PersistentClient` + HNSW，向量在 chroma 的 index 文件里，Rust 不能无损写那套二进制还保证 py 能接着搜。

本实现把向量和 metadata 放在同一目录下的 `chroma.sqlite3` 表 `event_memroy`（名字对齐 collection）。启动时若库里已有 Chroma 的 `embeddings` / `embedding_metadata` 且能读到向量，就导入。查询是全量精确 L2，记忆量很小，排序应与 HNSW 一致或更稳。

**已知缝：** 若旧库只有 HNSW 文件、sqlite 里没有向量，rs 读不到 py 已经写下的事件向量。ums/sms/status/sticker 不受影响。不要另起 `data/` 路径，不要改 collection 名。

## 代码布局

```
src/main.rs      找根目录、chdir、拉起 RPC 与主循环
src/config.rs    yaml 热加载、点号取值、Python format
src/log.rs       日志
src/util.rs      时间可读化、truncate、L2/cosine
src/filedb.rs    对齐 py FileDB
src/rpc.rs       aiorpcx 风格 JSON-RPC 客户端
src/types.rs     Message / poke 时间线
src/actions.rs   LLM actions 脏解析
src/memory.rs    ums/sms + 事件记忆 sqlite
src/sticker.rs   向量缓存与检索
src/format.rs    聊天记录/图片总结/摘要
src/chat.rs      chat() 与群 worker
```

改 rs 之前先把对应 py 片段读完。不确定的 API 不要猜。依赖能少则少，不把 `utils.py` 里用不到的工具搬过来。