#!/usr/bin/env bash
# CrossMail 本机开发环境（纯用户态，无需 sudo / 系统 gcc）
#   source scripts/dev-env.sh
# 工具链包装器随仓库提交，CI 与本机会话使用同一套脚本。
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TC="$ROOT/scripts/toolchain"
export CROSSMAIL_TOOLCHAIN="$TC"
export PATH="$TC:$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

# ziglang 可能装在 WorkBuddy 隔离 venv，也可能装在 CI 的系统 python
VENV_PY="$HOME/.workbuddy/binaries/python/envs/default/bin/python"
if [ -z "${CROSSMAIL_PYTHON:-}" ]; then
  if [ -x "$VENV_PY" ] && "$VENV_PY" -c 'import ziglang' 2>/dev/null; then
    export CROSSMAIL_PYTHON="$VENV_PY"
  else
    export CROSSMAIL_PYTHON="$(command -v python3)"
  fi
fi

# 仅当系统确实没有 cc/ar 时，才启用 zig 后备，避免污染有原生工具链的机器
if ! command -v gcc >/dev/null 2>&1 && ! command -v cc >/dev/null 2>&1; then
  export CC="$TC/cc"
  export AR="$TC/ar"
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$TC/cc"
fi

cd "$ROOT" || exit 1
