# cc-switch-reprefill-fix

Codex 桌面版 → 本程序 → Strata。这里只有两个补丁。Codex 源码不改。

上游是回环或私网地址才写旁路字段：`localhost`、`127.0.0.1`、`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`。`http://10.99.0.106:8005/v1` 会写。`https://api.openai.com` 不写，避免对方拒收未知 JSON 字段。

## Patch1 插话时间补丁

解决：同一条会话里，插话完成后的下一句整段重读。

Codex 在那一轮末尾追加一条 developer，种类是 `additional_content.codex_apps_client_time_context`（时区 + 当天日期）。入站前缀仍然成立。本程序把 Responses 转成 Chat 时，把后出现的 developer 收成 `role=system`，再抽进 `messages[0]`。系统提示词中间被改掉，Strata 的精确前缀从这里断开，后面全部重读。

插话那一拍本身不写这段。普通紧挨着的续写也不写。

### 怎么打

文件 `src-tauri/src/proxy/providers/transform_codex_chat.rs`。

`requires_leading_system_only`：模型名（去掉 `/` 前面的供应商）以 `minimax` 开头，或包含 `minimax-`，返回真。只有这些上游要求 system 只能出现在第一条，否则 2013。

`responses_to_chat_completions_with_reasoning` 在排完 messages 之后调用：

```rust
let messages = collapse_system_messages_to_head(messages, requires_leading_system_only(model));
```

`collapse_system_messages_to_head(messages, collapse_all)`：

- 从第一条开始，连续的 `role=system` 且 `content` 是字符串的，抽出来，用 `\n\n` 拼成一条，放回 `messages[0]`。开头这段每轮字节相同，可以合成一条。
- 遇到第一条非 system 之后，`passed_preamble = true`。之后的 system 留在原下标，不再抽走。时间块留在它出现的那一轮前面。
- `collapse_all == true`（MiniMax）时，后面的 system 也抽进第一条。这是供应商开关，默认关。

`append_responses_input_as_chat_messages` 里，还没收到结果的 tool_call 中间夹着的 developer 先放进 `deferred_systems`。tool 结果那条路径不放，所以批次中间不拆开。下一条 user / assistant，或者 input 收尾，即使还有 tool 一直没回结果，也把攒着的消息放出。留着不放会把这条消息丢掉。不抽进 `messages[0]`。

测试 `responses_request_to_chat_merges_mid_stream_system_into_head`：出现在几条 user 之后的 developer（例如 `Collaboration Mode`）必须留在原下标。`messages[0]` 只含它之前的 system 文本。

### 和 Strata 的配合

只改本程序不够。这套权重的 `chat_template.jinja` 在消息循环里对非第一条 system 执行 `raise_exception('System message must be at the beginning.')`。时间块留在原位之后，请求会直接报错。

模板改法：删掉这句 `raise_exception`。循环里遇到非第一条的 system，按普通消息原位输出：

```text
<|im_start|>system
……这块 developer 文本……
<|im_end|>
```

开头那条 system 的渲染不动。前缀从第一条 `<|im_end|>` 之后才开始长。4090 上这份模板已经改过。3080 复刻引擎补丁时，如果用的是同一份未改模板，这一处也要改，否则补丁 1 发出去的请求被模板拒绝。

Strata 的插话停放（`same_chat_rewind`）是另一件事，在引擎里，不在本程序。本补丁只保证前缀不被时间块改写。

## Patch2 旁路请求补丁

解决：compact 和 subagent 的内部请求把停放槽占满。subagent 正文要留 1 格，来回切能换回。

Strata 只看渲染后的 token。compact 换成摘要，subagent 是另一条前缀，都和「另开一条会话」一样。没有线程号时，同一条 subagent 里每次换正文都新停一格。有线程号之后，同一条只留最新的那一格。

标记在 Codex 请求里，转发出去之前读。读的是 `client_metadata["x-codex-turn-metadata"]`（JSON 字符串）。正文没有时，用请求头 `x-codex-turn-metadata`。

旁路（`ephemeral`）为真，满足任一：

- `request_kind` 是 `compaction`、`prewarm`、`memory`
- `thread_source` 是 `thread_title`
- `input` 里有 `type=compaction_trigger`（`codex_compaction::is_compaction_request`）

`parent_thread_id` 和 `subagent_kind` 不构成旁路。subagent 正文是真实回合，用它自己的 `thread_id` 占 1 格。

`strata_conversation` 取值顺序：turn metadata 的 `thread_id`，其次 `client_metadata.thread_id`，其次 `client_metadata.session_id`。只保留长度 1 到 128、字符属于 `[A-Za-z0-9._:-]` 的值。子线程的 id 是子线程自己的；父对话停进槽时用的是父请求结束时记下的 id。

### 怎么打

新文件 `src-tauri/src/proxy/providers/strata_park.rs`。`mod.rs` 加 `pub(crate) mod strata_park;`。

`hint(body, turn_metadata_header) -> StrataParkHint { conversation, ephemeral }`。

`apply(base_url, body, hint) -> bool`：上游不是回环或私网，或既没有会话 id 也不旁路，返回 false，正文逐字节不变。否则写入：

- 有会话 id：`strata_conversation` = 该字符串
- 旁路：`strata_checkpoint` = `false`。普通回合不写这个字段

`src-tauri/src/proxy/forwarder.rs` 的 `forward`，在 `let mut request_body = if codex_responses_to_chat` 之前，仅 `AppType::Codex`：

```rust
let strata_park_hint = Some(strata_park::hint(
    &mapped_body,
    headers.get("x-codex-turn-metadata").and_then(|v| v.to_str().ok()),
));
```

必须在 `prepare_native_third_party_request` 改写压缩条目之前读。写出放在那段改写之后，对最终的 `request_body` 调用 `strata_park::apply(&base_url, &mut request_body, hint)`。Chat 转换和 Responses 原样转发都走这里。当前 4090 供应商 `wire_api = "responses"`，走原样转发，字段加在 Responses JSON 上，不是 Chat JSON。

日志一行：`[Strata] park hint ephemeral=true|false conversation=true|false`。不打印 id。

单测在 `strata_park.rs` 底部。`cargo test --manifest-path src-tauri/Cargo.toml --lib strata_park`。

### 和 Strata 的配合

Strata 认这两个字段，不认 Codex 的 metadata。引擎协议是 GEN 行上的键：

| 本程序写出的 JSON | `serve/server.py` 的 `sampling_keys` | 引擎 GEN 键 |
|---|---|---|
| `"strata_checkpoint": false` | 已有，拼 `ckpt=0` | `ckpt=0`：这条结束后 `live_ok=false`，下一条到来时不停它 |
| `"strata_conversation": "<thread id>"` | 拼 `conv=<id>`，同样的字符集 | 停放时写到快照的 `conv`；下一条真实回合 id 相同且前缀对不上，丢掉这份快照 |

父对话和 subagent 正文都是真实回合，不带 `strata_checkpoint: false`。离开一条去另一条时，离开的那条停进 1 格，带上它自己的线程号。同一条接着说不新占格。同一条被 compact 换成新正文时，引擎在匹配之前丢掉这个线程号的旧快照，还是 1 格。切回去时新提示词延长已停放的 token，从这一格换回显存。

compact、预热、记忆、标题这些旁路自己 `ckpt=0`，不入槽。它到来时如果上一条是真实会话，上一条照旧停进 1 格。

没有 `strata_conversation` 时，`ckpt=0` 仍然生效，旁路不占格，但压缩前的旧快照不会被对掉，要等槽满才被挤走。

插话不走这个标记。插话的请求看起来和普通续写一样，只是砍了没写完的 assistant。那一格由 Strata 的 `same_chat_rewind` 负责。

## Patch3 前缀钉住

解决：同一条线程下一拍只多了一句，但开头的时间或工具顺序变了，token 从第一个就对不上，于是整段重读。

只对真实回合做。旁路（compact 触发、预热、记忆、标题）不改、不记。上游不是回环或私网时不做。

`src-tauri/src/proxy/providers/strata_prefix.rs`。`stabilize(base_url, body, conversation_id)` 在 `strata_park::apply` 之后调用。

按线程号记住上一拍发出去的 `messages`（没有则用 `input`）和 `tools`，最多 16 条。下一拍：

- 去掉 `<codex_apps_client_time_context>`
- 工具按名字排序，对象键排序
- 从开头逐条比对。对得上的用上一拍的原文，后面的新消息接上去
- 工具集合没变就用上一拍那份
- 旧消息被改掉（compact、用户改历史）时，从第一条不同的消息起改用新正文

日志一行：`[Strata] prefix kept=N`。N 是沿用的消息条数。不打印正文。

compact 换成的新正文对不上旧 token，这一次会整段读。读完仍是原来那一格。这不是意外重读。

## Mac 编译

这台 Linux 编不出 Mac 包。在 Mac 上，仓库就是这一份（含上面两个补丁）：

```bash
# Xcode、Rust（版本以 rust-toolchain.toml 的 channel 为准）、Node 20、pnpm 10.12.3
corepack enable
pnpm install --frozen-lockfile
pnpm tauri build
```

产物：

- `src-tauri/target/release/bundle/dmg/*.dmg`
- `src-tauri/target/release/bundle/macos/CC Switch.app`

装好后供应商 `base_url` 指向 4090 的私网地址（现在是 `http://10.99.0.106:8005/v1`）。指到公网域名时，补丁 2 的两个字段不会写。

## Linux 编译

```bash
# 系统包：build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev patchelf
corepack enable
pnpm install --frozen-lockfile
CI=true pnpm tauri build
```

产物目录 `src-tauri/target/release/bundle/`（`deb/`、`rpm/`、`appimage/`）。
