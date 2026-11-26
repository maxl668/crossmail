#!/usr/bin/env bash
# CrossMail 多架构编译链路（S0-05 / S0-06 / S0-07 + S1 JMAP / S2 IMAP / S3 同步引擎 / S4 业务层交叉验证）
# 用法: bash scripts/build-cross.sh [target ...]
#
# 双 ABI 分工（重要）：
#   *-pc-windows-msvc    : 纯 Rust crate 的静态库（crossmail-core，S0 验收产物）
#   *-pc-windows-gnullvm : 含 C 依赖（ring/rusqlite）的协议层，zig 自带 MinGW libc 头
#   aarch64-linux-android: NDK clang，产出 .a / .so
# MSVC 目标不编协议层：zig 的 MSVC ABI 不带 libc 头，本机无 Windows SDK / clang-cl。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=dev-env.sh
source "$ROOT/scripts/dev-env.sh"
TC="$CROSSMAIL_TOOLCHAIN"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
NDK="${ANDROID_NDK_HOME:-$HOME/.local/opt/android-ndk-r27c}"
NDK_BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"

TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  TARGETS=(
    x86_64-unknown-linux-gnu
    x86_64-pc-windows-msvc
    aarch64-pc-windows-msvc
    x86_64-pc-windows-gnullvm
    aarch64-pc-windows-gnullvm
    aarch64-linux-android
  )
fi

for target in "${TARGETS[@]}"; do
  echo "==> rustup target add $target"
  rustup target add "$target" || { echo "[WARN] target $target 不可用，跳过"; continue; }

  echo "==> cargo build --target $target"
  (
    case "$target" in
      x86_64-unknown-linux-gnu)
        "$CARGO" build -p crossmail-core -p crossmail-storage -p crossmail-jmap -p crossmail-imap -p crossmail-sync -p crossmail-service -p crossmail-sanitize -p crossmail-obs -p crossmail-app --release
        ;;
      x86_64-pc-windows-msvc)
        export CROSSMAIL_ZIG_TARGET=x86_64-windows-msvc
        export CC_x86_64_pc_windows_msvc="$TC/cc"
        export AR_x86_64_pc_windows_msvc="$TC/zig-ar"
        export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$TC/zig-link"
        "$CARGO" build -p crossmail-core --target "$target" --release
        ;;
      aarch64-pc-windows-msvc)
        export CROSSMAIL_ZIG_TARGET=aarch64-windows-msvc
        export CC_aarch64_pc_windows_msvc="$TC/cc"
        export AR_aarch64_pc_windows_msvc="$TC/zig-ar"
        export CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER="$TC/zig-link"
        "$CARGO" build -p crossmail-core --target "$target" --release
        ;;
      x86_64-pc-windows-gnullvm)
        export CROSSMAIL_ZIG_TARGET=x86_64-windows-gnu
        export CC_x86_64_pc_windows_gnullvm="$TC/cc"
        export AR_x86_64_pc_windows_gnullvm="$TC/zig-ar"
        export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER="$TC/zig-link"
        "$CARGO" build -p crossmail-core -p crossmail-storage -p crossmail-jmap -p crossmail-imap -p crossmail-sync -p crossmail-sanitize -p crossmail-obs -p crossmail-app --target "$target" --release
        ;;
      aarch64-pc-windows-gnullvm)
        export CROSSMAIL_ZIG_TARGET=aarch64-windows-gnu
        export CC_aarch64_pc_windows_gnullvm="$TC/cc"
        export AR_aarch64_pc_windows_gnullvm="$TC/zig-ar"
        export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER="$TC/zig-link"
        "$CARGO" build -p crossmail-core -p crossmail-storage -p crossmail-jmap -p crossmail-imap -p crossmail-sync -p crossmail-sanitize -p crossmail-obs -p crossmail-app --target "$target" --release
        ;;
      aarch64-linux-android)
        export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android29-clang"
        export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
        export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android29-clang"
        "$CARGO" build -p crossmail-core --target "$target" --release
        "$CARGO" build -p crossmail-ffi --target "$target" --release
        ;;
      *)
        "$CARGO" build -p crossmail-core --target "$target" --release
        ;;
    esac
  ) || { echo "[FAIL] $target 编译失败"; exit 1; }
  echo "[OK] $target"
done

echo "==> 产物清单"
find "$ROOT/target" -maxdepth 3 -type f \
  \( -name 'crossmail_core.lib' -o -name 'libcrossmail_core.a' -o -name 'libcrossmail_ffi.so' \) \
  -printf '%p (%k KB)\n' | sort
