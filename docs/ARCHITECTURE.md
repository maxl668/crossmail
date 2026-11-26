# 架构说明

## 分层

```
┌──────────────────────────────────────────────┐
│  UI 层（两条独立路线，共用核心）              │
│  ├─ Tauri v2 + React / TypeScript            │
│  └─ C# WinUI 3 + UniFFI FFI                  │
├──────────────────────────────────────────────┤
│  命令层 / FFI 层                              │
│  ├─ src-tauri/src/main.rs（命令注册）         │
│  └─ crates/ffi + win/bindings（C# 绑定）      │
├──────────────────────────────────────────────┤
│  应用组装层  crates/app                        │
│  └─ Backend / BuildBackend：账号、同步、发信   │
├──────────────────────────────────────────────┤
│  业务层  crates/service / sync / sanitize      │
├──────────────────────────────────────────────┤
│  协议层  crates/jmap / imap                    │
├──────────────────────────────────────────────┤
│  存储与模型  crates/storage / core / obs       │
└──────────────────────────────────────────────┘
```

## 数据流

1. 用户在 UI 选择账号 → 命令层调用 `open_engine`（注入持久化主密钥）
2. 同步引擎通过 JMAP 或 IMAP 拉取增量 → 解析邮件头与正文 → 写入 SQLite
3. 推送订阅在后台运行，收到 `Synced { inserted > 0 }` 事件时触发新邮件提醒
4. UI 读取本地库展示邮件列表与正文，时间字段由展示层转换到浏览器/系统本地时区

## 关键设计

- **UTC 单一真值**：邮件时间在协议解析阶段统一转为 UTC，存储为 RFC3339；UI 展示时才转本地时区。发信时 `Date` 头使用 UTC + `+0000`，与存储自洽。
- **主密钥持久化**：首次启动生成 32 字节主密钥并保存在用户私有目录，注入每个 `EngineConfig`；否则每次打开引擎重新生成会导致凭据不可解。
- **ABI 稳定边界**：`crates/ffi/src/crossmail_ffi.udl` 是 Rust 与 C# 的 ABI 契约。修改签名后必须重新生成 Python 绑定与 4 个 `.g.cs`，并同步更新 `EntryPoint` 断言。
- **UID 作用域**：邮件 UID 只在文件夹内唯一，数据库唯一约束为 `(account_id, mailbox_id, uid)`。

## 两条消费线的关系

两条线是**并存**而非替代关系：

| 维度 | Tauri 线 | C#/FFI 线 |
|---|---|---|
| 技术栈 | Tauri v2 + React/TS | WinUI 3 + UniFFI |
| 位置 | `platform/win-ui/` | `win/` |
| 交付形态 | 绿色 zip（单 exe + 说明） | 绿色 zip（无需 MSIX 安装） |
| 核心依赖 | 同一 Rust workspace | 同一 Rust workspace |

任何改动到 Rust 核心时，两条线都必须重新验证。
