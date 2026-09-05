#!/bin/bash
# Build the Android demo APK from scratch.
# Usage: ./build_apk.sh [install]
set -euo pipefail

ANDROID_HOME="${ANDROID_HOME:-$HOME/android-sdk}"
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/25.2.9519653}"
PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/build-tools/33.0.2:$ANDROID_HOME/emulator:$PATH"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="/tmp/android_apk_build"
rm -rf "$BUILD_DIR" && mkdir -p "$BUILD_DIR"/{classes,dex,staging}

# 1. Build Rust .so
echo "==> Building Rust .so for x86_64..."
cd "$PROJECT_ROOT"
cargo ndk -t x86_64 build --release -p rustxwidgets_android_demo 2>&1 | tail -1

# 2. Compile Java sources
echo "==> Compiling Java..."
javac -d "$BUILD_DIR/classes" -source 8 -target 8 \
  -cp "$ANDROID_HOME/platforms/android-33/android.jar" \
  "$SCRIPT_DIR/app/src/main/java/com/example/"*.java 2>&1 | grep -v warning

# 3. Convert to DEX
echo "==> Converting to DEX..."
d8 --release --lib "$ANDROID_HOME/platforms/android-33/android.jar" \
  --output "$BUILD_DIR/dex" "$BUILD_DIR/classes/com/example/"*.class 2>&1

# 4. Package resources
echo "==> Packaging resources..."
aapt2 compile --dir "$SCRIPT_DIR/app/src/main/res" -o "$BUILD_DIR/resources.zip" 2>&1 | grep -v "^INFO\|^$" || true

# 5. Link base APK
echo "==> Linking APK..."
aapt2 link -o "$BUILD_DIR/unsigned.apk" \
  -I "$ANDROID_HOME/platforms/android-33/android.jar" \
  --manifest "$SCRIPT_DIR/app/src/main/AndroidManifest.xml" \
  "$BUILD_DIR/resources.zip" 2>&1

# 6. Add DEX and native lib
echo "==> Adding DEX and native lib..."
mkdir -p "$BUILD_DIR/staging/lib/x86_64"
unzip -qo "$BUILD_DIR/unsigned.apk" -d "$BUILD_DIR/staging"
cp "$BUILD_DIR/dex/classes.dex" "$BUILD_DIR/staging/"
cp "$PROJECT_ROOT/target/x86_64-linux-android/release/librustxwidgets_android_demo.so" \
  "$BUILD_DIR/staging/lib/x86_64/"
cd "$BUILD_DIR/staging" && zip -qr "$BUILD_DIR/unsigned_with_libs.apk" . && cd /tmp

# 7. Align and sign
echo "==> Aligning and signing..."
zipalign -v -p 4 "$BUILD_DIR/unsigned_with_libs.apk" "$BUILD_DIR/aligned.apk" 2>&1 | tail -1

KEYSTORE="$BUILD_DIR/debug.keystore"
[ -f "$KEYSTORE" ] || keytool -genkey -v -keystore "$KEYSTORE" -alias androiddebugkey \
  -keyalg RSA -keysize 2048 -validity 10000 -storepass android -keypass android \
  -dname "CN=, OU=, O=, L=, S=, C=" 2>&1 >/dev/null

apksigner sign --ks "$KEYSTORE" --ks-pass pass:android --ks-key-alias androiddebugkey \
  --v1-signing-enabled true --v2-signing-enabled true --min-sdk-version 24 \
  --out "$BUILD_DIR/signed.apk" "$BUILD_DIR/aligned.apk" 2>&1

echo "==> APK built: $BUILD_DIR/signed.apk"

# Optionally install and run
if [ "${1:-}" = "install" ]; then
  echo "==> Installing..."
  adb install "$BUILD_DIR/signed.apk" 2>&1 | tail -1
  echo "==> Launching..."
  adb shell am start -n com.example/.MainActivity
fi
