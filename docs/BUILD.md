# 构建指南

## 环境要求

- Rust（见 `rust-toolchain.toml`）
- Node.js + npm（Tauri 线）
- .NET 8 + Windows App SDK（C# 线）
- Windows 上构建 C# 需 x64 MSVC 工作负载

## Rust 核心

```bash
cargo test                              # 默认成员单元测试
cargo test --workspace --all-targets     # 含 ffi 全量
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/build-cross.sh             # 三平台编译链路验证
```

交叉编译工具链包装器在 `scripts/toolchain/`，可 `source scripts/dev-env.sh` 载入。

## Tauri 桌面客户端

```bash
cd platform/win-ui
npm install
npx tsc --noEmit                        # 类型检查
npx tauri build --no-bundle             # 构建 exe（不打包安装包）
```

产物：`src-tauri/target/release/crossmail-win-ui.exe`。

## C# WinUI 3 客户端

```powershell
cd win
dotnet build app/CrossMail.App/CrossMail.App.csproj -c Release -p:Platform=x64
.\app\CrossMail.App\bin\x64\Release\net8.0-windows10.0.19041.0\CrossMail.App.exe --window-smoke
```

绿色包脚本：

```powershell
powershell.exe -ExecutionPolicy Bypass -File scripts/build-portable.ps1 -Platform x64 -OutputDir dist\portable
```

## 绑定重新生成

仅当 `crates/ffi/src/crossmail_ffi.udl` 变更时需要：

```bash
cargo build -p crossmail-ffi
cargo run -p crossmail-ffi --bin uniffi-bindgen -- generate --library <ffi-library> --language python --out-dir <dir>
python win/tools/gen_csharp_bindings.py
powershell.exe -ExecutionPolicy Bypass -File win/scripts/check-csharp-bindings.ps1
```

重新生成后必须同步更新 `EntryPoint` 断言数量。
