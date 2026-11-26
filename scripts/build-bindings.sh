#!/usr/bin/env bash
# 生成 CrossMail 多语言绑定并校验符号完整性（S5-01）
#
#   build/bindings/kotlin/   Kotlin 绑定（Android / JVM）
#   build/bindings/swift/    Swift 绑定（iOS / macOS）
#   build/bindings/python/   Python 绑定（脚本与自测）
#
# 校验逻辑：绑定生成"成功"不等于"能用"——UDL 改了但没重新生成、或某个方法被
# 漏掉，生成器照样返回 0。所以这里按契约逐项 grep 关键符号，缺一个就失败。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=dev-env.sh
source "$ROOT/scripts/dev-env.sh"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
UDL="$ROOT/crates/ffi/src/crossmail_ffi.udl"
OUT="$ROOT/build/bindings"

[ -f "$UDL" ] || { echo "[FAIL] 未找到契约 $UDL"; exit 1; }

echo "==> 构建 uniffi-bindgen"
"$CARGO" build -q -p crossmail-ffi --features bindgen --bin uniffi-bindgen
BINDGEN="$ROOT/target/debug/uniffi-bindgen"

for lang in kotlin swift python; do
  echo "==> 生成 $lang 绑定"
  rm -rf "$OUT/$lang"
  mkdir -p "$OUT/$lang"
  "$BINDGEN" generate "$UDL" --language "$lang" --out-dir "$OUT/$lang" --no-format
done

# Python 绑定是 ctypes 加载 .so 的：把主机 cdylib 摆到绑定目录旁，
# 否则脚本里 `import crossmail_ffi` 会在 LoadLibrary 阶段直接失败。
# （每次生成都会 rm -rf 输出目录，所以这一步必须放在生成之后。）
echo "==> 构建主机 cdylib 供 Python 绑定加载"
"$CARGO" build -q -p crossmail-ffi --release
cp "$ROOT/target/release/libcrossmail_ffi.so" "$OUT/python/libuniffi_crossmail_ffi.so"

KT="$OUT/kotlin/uniffi/crossmail_ffi/crossmail_ffi.kt"
SWIFT_DIR="$OUT/swift"
PY="$OUT/python/crossmail_ffi.py"

fail=0
expect() { # expect <file> <pattern> <label>
  if grep -qF "$2" "$1" 2>/dev/null; then
    printf '  [OK]   %s\n' "$3"
  else
    printf '  [FAIL] %s（缺 %s）\n' "$3" "$2"
    fail=1
  fi
}

echo "==> 校验 Kotlin 绑定"
[ -s "$KT" ] || { echo "[FAIL] Kotlin 绑定未生成"; exit 1; }
expect "$KT" "data class EmailDto"                "EmailDto"
expect "$KT" "data class EmailPageDto"            "EmailPageDto"
expect "$KT" "data class AccountDto"              "AccountDto"
expect "$KT" "data class NewAccountDto"           "NewAccountDto"
expect "$KT" "data class AccountPatchDto"         "AccountPatchDto"
expect "$KT" "data class MailboxDto"              "MailboxDto"
expect "$KT" "data class EngineConfig"            "EngineConfig"
expect "$KT" "data class CacheStatusDto"          "CacheStatusDto"
expect "$KT" "sealed class FlagDto"               "FlagDto(sealed)"
expect "$KT" "sealed class CrossMailException"    "CrossMailException"
expect "$KT" "enum class ProtocolKindDto"         "ProtocolKindDto"
expect "$KT" "enum class SecurityModeDto"         "SecurityModeDto"
expect "$KT" "enum class MailboxRoleDto"          "MailboxRoleDto"
expect "$KT" "enum class SearchScopeDto"          "SearchScopeDto"
expect "$KT" "interface CrossMailEngineInterface" "CrossMailEngine"
expect "$KT" 'fun `openEngine`(' "openEngine()"
expect "$KT" 'fun `ensureSystemMailboxes`(' "ensureSystemMailboxes()"
expect "$KT" 'fun `listEmails`(' "listEmails()"
expect "$KT" 'fun `readBody`(' "readBody()"
expect "$KT" 'fun `saveDraft`(' "saveDraft()"
expect "$KT" 'fun `markSent`(' "markSent()"
expect "$KT" 'fun `search`(' "search()"
expect "$KT" 'fun `cacheStatus`(' "cacheStatus()"
expect "$KT" 'fun `enforceCacheQuota`(' "enforceCacheQuota()"
expect "$KT" 'fun `protocolDisplayName`(' "protocolDisplayName()"
# S6：安全渲染与可观测
expect "$KT" "data class RenderedBodyDto"          "RenderedBodyDto"
expect "$KT" "data class SenderTrustDto"           "SenderTrustDto"
expect "$KT" "data class AllowanceDto"             "AllowanceDto"
expect "$KT" "data class LogStatusDto"             "LogStatusDto"
expect "$KT" 'fun `renderBody`('                   "renderBody()"
expect "$KT" 'fun `senderTrust`('                  "senderTrust()"
expect "$KT" 'fun `allowSenderRemoteImages`('      "allowSenderRemoteImages()"
expect "$KT" 'fun `revokeSenderRemoteImages`('     "revokeSenderRemoteImages()"
expect "$KT" 'fun `listRemoteAllowances`('         "listRemoteAllowances()"
expect "$KT" 'fun `exportDiagnostics`('            "exportDiagnostics()"
expect "$KT" 'fun `setLogLevel`('                  "setLogLevel()"
expect "$KT" 'fun `setProtocolPayloadLogging`('    "setProtocolPayloadLogging()"
expect "$KT" 'fun `logStatus`('                    "logStatus()"
expect "$KT" 'fun `rotateLogs`('                   "rotateLogs()"
# S6-16：同步与推送（callback interface + 角标 DTO + 取消句柄）
expect "$KT" "data class SyncReportDto"          "SyncReportDto"
expect "$KT" "data class MailboxUnreadDto"       "MailboxUnreadDto"
expect "$KT" "data class PushEventDto"           "PushEventDto"
expect "$KT" "enum class PushEventKindDto"       "PushEventKindDto"
expect "$KT" "public interface PushObserver"     "PushObserver(callback)"
expect "$KT" "public interface PushSubscriptionInterface" "PushSubscription"
expect "$KT" 'fun `subscribePush`('             "subscribePush()"
expect "$KT" 'fun `syncAccount`('               "syncAccount()"

echo "==> 校验 Swift 绑定"
[ -s "$SWIFT_DIR/crossmail_ffi.swift" ] || { echo "[FAIL] Swift 绑定未生成"; exit 1; }
expect "$SWIFT_DIR/crossmail_ffi.swift" "struct EmailDto"            "EmailDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public protocol CrossMailEngineProtocol" "CrossMailEngine"
expect "$SWIFT_DIR/crossmail_ffi.swift" "func openEngine("           "openEngine()"
expect "$SWIFT_DIR/crossmail_ffi.swift" "enum CrossMailError"        "CrossMailError"
expect "$SWIFT_DIR/crossmail_ffi.swift" "struct RenderedBodyDto"     "RenderedBodyDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "func renderBody("           "renderBody()"
expect "$SWIFT_DIR/crossmail_ffi.swift" "func exportDiagnostics("    "exportDiagnostics()"
# S6-16：同步与推送
expect "$SWIFT_DIR/crossmail_ffi.swift" "public struct SyncReportDto"   "SyncReportDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public struct MailboxUnreadDto" "MailboxUnreadDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public struct PushEventDto"     "PushEventDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public enum PushEventKindDto"   "PushEventKindDto"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public protocol PushObserver"   "PushObserver(callback)"
expect "$SWIFT_DIR/crossmail_ffi.swift" "public protocol PushSubscriptionProtocol" "PushSubscription"
expect "$SWIFT_DIR/crossmail_ffi.swift" "func subscribePush("           "subscribePush()"
expect "$SWIFT_DIR/crossmail_ffi.swift" "func syncAccount("             "syncAccount()"
[ -f "$SWIFT_DIR/crossmail_ffiFFI.h" ] || { echo "  [FAIL] 缺 C 头文件 crossmail_ffiFFI.h"; fail=1; }
[ -f "$SWIFT_DIR/crossmail_ffiFFI.modulemap" ] || { echo "  [FAIL] 缺 modulemap"; fail=1; }

echo "==> 校验 Python 绑定"
[ -s "$PY" ] || { echo "[FAIL] Python 绑定未生成"; exit 1; }
expect "$PY" "class EmailDto"            "EmailDto"
expect "$PY" "class CrossMailEngine"     "CrossMailEngine"
expect "$PY" "def open_engine("          "open_engine()"
expect "$PY" "class RenderedBodyDto"     "RenderedBodyDto"
expect "$PY" "def render_body("          "render_body()"
expect "$PY" "def sender_trust("         "sender_trust()"
expect "$PY" "def allow_sender_remote_images(" "allow_sender_remote_images()"
expect "$PY" "def export_diagnostics("   "export_diagnostics()"
expect "$PY" "def set_log_level("        "set_log_level()"
expect "$PY" "def log_status("           "log_status()"
expect "$PY" "def rotate_logs("          "rotate_logs()"
# S6-16：同步与推送
expect "$PY" "class SyncReportDto"       "SyncReportDto"
expect "$PY" "class MailboxUnreadDto"    "MailboxUnreadDto"
expect "$PY" "class PushEventDto"        "PushEventDto"
expect "$PY" "class PushEventKindDto"    "PushEventKindDto"
expect "$PY" "class PushObserver"        "PushObserver(callback)"
expect "$PY" "class PushSubscription"    "PushSubscription"
expect "$PY" "def subscribe_push("       "subscribe_push()"
expect "$PY" "def sync_account("         "sync_account()"

echo
if [ "$fail" -ne 0 ]; then
  echo "[FAIL] 绑定符号校验未通过"
  exit 1
fi
echo "==> 绑定产物"
for f in "$KT" "$SWIFT_DIR/crossmail_ffi.swift" "$PY"; do
  printf '  %-64s %s\n' "${f#"$ROOT"/}" "$(wc -l <"$f") 行"
done
echo "[OK] 三语言绑定生成并通过符号校验"
