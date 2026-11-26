#!/usr/bin/env bash
# 生成 Android 可导入产物（S0-06）：
#   build/android/crossmail-core.aar          原生库 AAR（jni/arm64-v8a/libcrossmail_ffi.so）
#   build/android/kotlin-bindings/            uniffi 生成的 Kotlin 绑定源码
#   build/android/crossmail-core.aar.sha256   校验和
# 说明：本机无 JDK/Kotlin/Gradle，classes.jar 为空壳；Kotlin 绑定由 App 模块
#       直接以源码依赖编译（uniffi 官方推荐方式），完整 classes.jar 由 Android Gradle 构建产出。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=dev-env.sh
source "$ROOT/scripts/dev-env.sh"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

OUT="$ROOT/build/android"
STAGE="$OUT/.stage"
SO="$ROOT/target/aarch64-linux-android/release/libcrossmail_ffi.so"
UDL="$ROOT/crates/ffi/src/crossmail_ffi.udl"

if [ ! -f "$SO" ]; then
  echo "==> Android .so 缺失，先执行交叉编译"
  bash "$ROOT/scripts/build-cross.sh" aarch64-linux-android
fi
[ -f "$SO" ] || { echo "[FAIL] 未找到 $SO"; exit 1; }

echo "==> 复用统一绑定产物（S5-01 起由 build-bindings.sh 生成三语言）"
KT_FILE="$ROOT/build/bindings/kotlin/uniffi/crossmail_ffi/crossmail_ffi.kt"
if [ ! -s "$KT_FILE" ]; then
  bash "$ROOT/scripts/build-bindings.sh"
fi
[ -s "$KT_FILE" ] || { echo "[FAIL] Kotlin 绑定未生成"; exit 1; }

echo "==> 组装 AAR"
rm -rf "$OUT/.stage" "$OUT/kotlin-bindings"
mkdir -p "$STAGE/jni/arm64-v8a" "$STAGE/res/values" "$STAGE/META-INF" "$STAGE/.jarstage/META-INF"
cp "$SO" "$STAGE/jni/arm64-v8a/libcrossmail_ffi.so"
mkdir -p "$OUT/kotlin-bindings"
cp -r "$ROOT/build/bindings/kotlin/uniffi" "$OUT/kotlin-bindings/"

cat > "$STAGE/AndroidManifest.xml" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.crossmail.core">
    <uses-sdk android:minSdkVersion="29" />
</manifest>
XML
: > "$STAGE/R.txt"
: > "$STAGE/proguard.txt"
printf 'Manifest-Version: 1.0\nCreated-By: CrossMail build-aar\n' \
  > "$STAGE/.jarstage/META-INF/MANIFEST.MF"
(cd "$STAGE/.jarstage" && zip -q -r "$STAGE/classes.jar" META-INF)

rm -f "$OUT/crossmail-core.aar"
(cd "$STAGE" && zip -q -r -X "$OUT/crossmail-core.aar" \
  AndroidManifest.xml classes.jar R.txt proguard.txt jni res)
sha256sum "$OUT/crossmail-core.aar" | cut -d' ' -f1 > "$OUT/crossmail-core.aar.sha256"

echo "==> 校验 AAR 内容"
unzip -l "$OUT/crossmail-core.aar" | tail -12
echo "[OK] $OUT/crossmail-core.aar"
