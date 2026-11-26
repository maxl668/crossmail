# CrossMail

本地优先（local-first）的跨平台原生邮件客户端内核与 Windows 桌面实现。Rust 核心库通过 JMAP 与 IMAP/SMTP 双协议收发邮件，UI 层完全解耦，提供两条并存的消费路线。

## 特性

- **双协议**：JMAP（RFC 8620/8621）与 IMAP4rev1/SMTP，同一套领域模型
- **本地优先**：SQLite 存储 + 加密凭据保险库（ChaCha20），数据留在用户设备
- **双消费层**：Tauri v2 + React/TypeScript，以及 C# WinUI 3 + UniFFI FFI，共用同一个 Rust 核心
- **跨平台内核**：Windows x86-64 / ARM64、Linux、Android ARM64（核心库）
- **双态新邮件提醒**（v1.7）：最小化时系统托盘气泡，浏览时应用内醒目卡片
- **时区正确**（v1.7）：邮件时间全链路 UTC 存储，展示层按本地时区渲染

## 仓库布局

```
.
├── crates/                     Rust workspace（核心库）
│   ├── core                    统一领域模型 / 错误 / MailProvider 契约
│   ├── storage                 SQLite 迁移框架与 CRUD
│   ├── jmap                    JMAP 协议实现
│   ├── imap                    IMAP + SMTP 协议实现
│   ├── sync                    增量同步引擎
│   ├── service                 账号 / 凭据 / 业务编排
│   ├── sanitize                内容清洗与钓鱼检测
│   ├── obs                     可观测性
│   ├── app                     应用组装层（Backend / BuildBackend）
│   └── ffi                     UniFFI 绑定层
├── platform/win-ui/            Tauri v2 + React 桌面客户端
│   ├── src/                    React/TypeScript 前端
│   └── src-tauri/              Tauri 命令层 + 托盘气泡模块
├── win/                        C# WinUI 3 + UniFFI FFI 桌面客户端
│   ├── app/CrossMail.App       WinUI 3 应用
│   ├── bindings/               C# FFI 绑定（含 4 个 .g.cs 生成文件）
│   ├── abi/                    ABI 符号表
│   ├── scripts / tools / smoke 构建、绑定生成与冒烟验证
├── docs/                       架构、构建与开发说明
└── scripts/                    Rust 核心构建脚本
```

## 快速开始

### Rust 核心

```bash
# 单元测试（默认成员）
cargo test

# 全 workspace 含 ffi
cargo test --workspace --all-targets

# 格式与静态检查
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Tauri 桌面客户端

```bash
cd platform/win-ui
npm install
npm run tauri dev
npm run tauri build
```

### C# WinUI 3 客户端

```powershell
# 在 win/ 目录下，使用 Release x64
dotnet build app/CrossMail.App/CrossMail.App.csproj -c Release -p:Platform=x64
```

## 文档

- [架构说明](docs/ARCHITECTURE.md) — 分层、数据流、双消费层
- [构建指南](docs/BUILD.md) — 两条线的构建与打包命令
- [变更日志](CHANGELOG.md) — 版本历史
- [路线图与已知限制](docs/ROADMAP.md) — 未完成项

## 安全

- 账号凭据加密后存入本地 SQLite，主密钥首次启动生成并保存在用户私有目录，不会随仓库分发。
- 仓库不包含任何真实凭据、密钥或测试账号；示例中的域名与邮箱均为 `example.com` 保留地址。
- 若发现敏感信息，请勿提交 issue 公开细节，直接联系维护者。

## 许可

本项目采用 MIT OR Apache-2.0 双许可，详见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)。
