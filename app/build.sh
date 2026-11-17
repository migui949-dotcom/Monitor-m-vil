#!/bin/bash
# Compila la app y genera el APK firmado (sin Gradle): cargo → aapt2 → zip → zipalign → apksigner
set -euo pipefail
cd "$(dirname "$0")"
D=$HOME/.local/share/android-dev
export JAVA_HOME=$D/jdk ANDROID_HOME=$D/sdk PATH=$HOME/.cargo/bin:$D/jdk/bin:$PATH
BT=$ANDROID_HOME/build-tools/35.0.0
PLAT=$ANDROID_HOME/platforms/android-35/android.jar
NDK=$ANDROID_HOME/ndk/29.0.14206865
export CC_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android29-clang
export AR_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar

SO=target/aarch64-linux-android/release/libmovil.so
if [ "${LOCAL:-0}" = 1 ]; then
    cargo build --release --target aarch64-linux-android
else
    # Compila en el Nobara (12 núcleos): manda el código, compila allí y trae la librería
    R=nobara; RD=movil-build/app
    RN='$HOME/.local/share/android-dev/android-ndk-r29/toolchains/llvm/prebuilt/linux-x86_64/bin'
    ssh $R "mkdir -p $RD"
    rsync -az --delete --exclude target --exclude build ./ "$R:$RD/"
    ssh $R "cd $RD && CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$RN/aarch64-linux-android29-clang \
        CC_aarch64_linux_android=$RN/aarch64-linux-android29-clang AR_aarch64_linux_android=$RN/llvm-ar \
        ~/.cargo/bin/cargo build --release --target aarch64-linux-android 2>&1" | grep -vE '^\s+(Compiling|Downloaded|Downloading|Locking|Adding|Updating)'
    test "${PIPESTATUS[0]}" = 0
    mkdir -p "$(dirname $SO)"
    rsync -az "$R:$RD/$SO" "$SO"
fi
OUT=build; rm -rf $OUT; mkdir -p $OUT/apk/lib/arm64-v8a
cp target/aarch64-linux-android/release/libmovil.so $OUT/apk/lib/arm64-v8a/
cp $NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so $OUT/apk/lib/arm64-v8a/ 2>/dev/null || true
# Capa Java mínima (MainActivity: pantalla inmersiva, permisos, pantalla encendida)
mkdir -p $OUT/classes $OUT/dex
javac -encoding UTF-8 -source 8 -target 8 -nowarn -Xlint:-options -cp $PLAT -d $OUT/classes $(find java -name '*.java')
$BT/d8 --release --min-api 29 --lib $PLAT --output $OUT/dex $(find $OUT/classes -name '*.class')
$BT/aapt2 link -o $OUT/base.apk --manifest AndroidManifest.xml -I $PLAT
python3 - "$OUT" <<'PY'
import sys, os, zipfile
out = sys.argv[1]
with zipfile.ZipFile(os.path.join(out, "base.apk"), "a", zipfile.ZIP_DEFLATED) as z:
    z.write(os.path.join(out, "dex", "classes.dex"), "classes.dex")
    for root, _, files in os.walk(os.path.join(out, "apk")):
        for f in files:
            full = os.path.join(root, f)
            z.write(full, os.path.relpath(full, os.path.join(out, "apk")))
PY
$BT/zipalign -f -p 4 $OUT/base.apk $OUT/aligned.apk
KS=$HOME/.local/share/movil-pantalla/movil.keystore
[ -f "$KS" ] || keytool -genkeypair -keystore "$KS" -storepass movilpantalla -keypass movilpantalla \
    -alias movil -keyalg RSA -keysize 3072 -validity 10000 -dname "CN=Movil Pantalla" >/dev/null 2>&1
$BT/apksigner sign --ks "$KS" --ks-pass pass:movilpantalla --out $OUT/movil.apk $OUT/aligned.apk
ls -la $OUT/movil.apk
