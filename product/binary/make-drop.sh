#!/bin/bash
# Assembles the binary delivery of the x-kernel VM package: one self-contained directory that an
# image build includes with a single inherit-product line (product.mk.template). It holds the
# programs and the control app built from source in an Android tree of the product's baseline,
# this repository's init script, instance config and SELinux policy sources, and the prebuilts
# that never come from source (the host driver's modules,
# the x-kernel guest images). MANIFEST.txt records the origin and sha256 of every file.
#
#   make-drop.sh --product-out DIR --prebuilts DIR --out DIR [--app-commit SHA] [--service-commit SHA]
#                [--baseline TEXT]
#
#   --product-out  the build's PRODUCT_OUT after `m xkernel_vm_service xkernel_vmctl XKernelVmControl`
#                  (for T11206 out_system/target/product/ussi_arm64): system_ext/bin/xkernel_* and
#                  system_ext/app/XKernelVmControl/XKernelVmControl.apk are taken from there
#   --prebuilts    modules/<each .ko of etc/modules.load>, kernel.bin, dummy.img,
#                  rootfs.img (the template, with xkload already added by guest/add-to-rootfs.sh)
#   --out          the delivery directory to create (must not exist, or be empty)
#   --app-commit   the t11206-pvm-app commit the control app was built from (recorded only)
#   --service-commit  this repository's commit the programs were built from; by default the HEAD of
#                  this checkout (a copy without .git, as in a build tree, needs the option)
#   --baseline     the Android baseline the programs were built for; by default the first of
#                  ro.system.build.fingerprint (system/build.prop), ro.system_ext.build.fingerprint
#                  (system_ext/etc/build.prop; Unisoc's system half writes only this one) and
#                  ro.system.build.id with ro.system.build.version.incremental
#
# The programs depend on platform libraries that are not stable across Android versions
# (libbinder_rpc_unstable): rebuild and redeliver whenever the product moves to a new baseline.
set -Eeuo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
product_out= prebuilts= out= app_commit=unknown service_commit= baseline=
while [ $# -gt 0 ]; do
    case "$1" in
    --product-out) product_out=$2; shift 2 ;;
    --prebuilts) prebuilts=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --app-commit) app_commit=$2; shift 2 ;;
    --service-commit) service_commit=$2; shift 2 ;;
    --baseline) baseline=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done
[ -n "$product_out" ] && [ -n "$prebuilts" ] && [ -n "$out" ] || { sed -n '9,29p' "$0" >&2; exit 2; }

fail() { echo "make-drop: $*" >&2; exit 1; }
need() { [ -f "$1" ] || fail "missing $1"; }
sha() { if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
# Shared libraries an ELF file names (DT_NEEDED), one per line.
needed() {
    if command -v readelf > /dev/null; then
        readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\].*/\1/p'
    else
        objdump -p "$1" | awk '/NEEDED/ {print $2}'
    fi
}
is_aarch64_elf() { [ "$(head -c 4 "$1" | od -An -c | tr -d ' ')" = 177ELF ] && [ "$(od -An -tu2 -j18 -N2 "$1" | tr -d ' ')" = 183 ]; }

if [ -d "$out" ] && [ -n "$(ls -A "$out")" ]; then fail "$out exists and is not empty"; fi

# The modules in load order, without comments (etc/modules.load).
modules=()
while read -r m; do modules+=("$m"); done < <(sed 's/#.*//' "$repo/etc/modules.load" | tr -s ' \t' '\n' | sed '/^$/d')
[ ${#modules[@]} -gt 0 ] || fail "no module in etc/modules.load"

programs=(xkernel_vm_service xkernel_crosvm_launcher xkernel_vm_loader xkernel_vmctl)
for p in "${programs[@]}"; do need "$product_out/system_ext/bin/$p"; done
app=$product_out/system_ext/app/XKernelVmControl/XKernelVmControl.apk
need "$app"
for m in "${modules[@]}"; do need "$prebuilts/modules/$m"; done
for f in kernel.bin dummy.img rootfs.img; do need "$prebuilts/$f"; done
extra=$(cd "$prebuilts/modules" && ls | grep -vxF -f <(printf '%s\n' "${modules[@]}") || true)
[ -z "$extra" ] || fail "modules/ holds files etc/modules.load does not list: $extra"

prop() { [ -f "$1" ] && sed -n "s/^$2=//p" "$1" | head -1 || true; }
if [ -z "$baseline" ]; then
    sys=$product_out/system/build.prop ext=$product_out/system_ext/etc/build.prop
    baseline=$(prop "$sys" 'ro\.system\.build\.fingerprint')
    [ -n "$baseline" ] || baseline=$(prop "$ext" 'ro\.system_ext\.build\.fingerprint')
    if [ -z "$baseline" ] && [ -n "$(prop "$sys" 'ro\.system\.build\.id')" ]; then
        baseline="$(prop "$sys" 'ro\.system\.build\.id')/$(prop "$sys" 'ro\.system\.build\.version\.incremental')"
    fi
    [ -n "$baseline" ] || fail "no --baseline and no build fingerprint or id in $sys or $ext"
fi
if [ -z "$service_commit" ]; then
    service_commit=$(git -C "$repo" rev-parse HEAD 2> /dev/null || echo unknown)
    if git -C "$repo" rev-parse HEAD > /dev/null 2>&1 && [ -n "$(git -C "$repo" status --porcelain)" ]; then
        service_commit="$service_commit (working tree modified)"
    fi
fi

mkdir -p "$out"/{bin,app,etc/init,modules,sepolicy}
for p in "${programs[@]}"; do cp "$product_out/system_ext/bin/$p" "$out/bin/$p"; done
for p in "${programs[@]}"; do is_aarch64_elf "$out/bin/$p" || fail "bin/$p is not an AArch64 ELF file"; done
cp "$app" "$out/app/XKernelVmControl.apk"
cp "$repo/etc/init/xkernel_vm_service.rc" "$out/etc/init/"
cp "$repo/etc/xkernel_vm.json" "$repo/etc/modules.load" "$out/etc/"
cp -R "$repo/sepolicy/private" "$out/sepolicy/"
find "$out/sepolicy" -name .DS_Store -delete
for m in "${modules[@]}"; do cp "$prebuilts/modules/$m" "$out/modules/"; done
for f in kernel.bin dummy.img rootfs.img; do cp "$prebuilts/$f" "$out/"; done
cp "$here/Android.bp.template" "$out/Android.bp"
sed "s|@MODULES@|${modules[*]}|" "$here/product.mk.template" > "$out/product.mk"

# Each program's shared libraries must be the ones Android.bp declares (bionic's libc, libm and
# libdl come with every binary): otherwise the image build's ELF check fails, or worse, a library
# the program needs is not installed. The exception are platform libraries whose visibility no
# module outside the platform may name; the platform image installs them anyway, and a program
# that needs one must have check_elf_files: false.
PLATFORM_ONLY_LIBS='libbinder_rpc_unstable'
elf_check_off() {
    awk -v name="\"$1\"" '
        /^cc_prebuilt_binary/ { inmod = 1; mine = 0 }
        inmod && $0 ~ "name: " name "," { mine = 1 }
        inmod && mine && /check_elf_files: false/ { found = 1 }
        inmod && /^}/ { inmod = 0 }
        END { exit found ? 0 : 1 }' "$out/Android.bp"
}
declared() {
    awk -v name="\"$1\"" '
        /^cc_prebuilt_binary/ { inmod = 1; mine = 0; inlibs = 0 }
        inmod && $0 ~ "name: " name "," { mine = 1 }
        inmod && /shared_libs: \[/ { inlibs = 1 }
        inmod && inlibs && mine { while (match($0, /"[^"]+"/)) { print substr($0, RSTART + 1, RLENGTH - 2); $0 = substr($0, RSTART + RLENGTH) } }
        inlibs && /\]/ { inlibs = 0 }
        inmod && /^}/ { inmod = 0 }' "$out/Android.bp" | sort -u
}
for p in "${programs[@]}"; do
    all=$(needed "$out/bin/$p" | { grep -vxE 'libc\.so|libm\.so|libdl\.so' || true; } | sed 's/\.so$//' | sort -u)
    want=$(echo "$all" | { grep -vxE "$PLATFORM_ONLY_LIBS" || true; })
    if [ "$all" != "$want" ] && ! elf_check_off "$p"; then
        fail "bin/$p needs a platform-only library ($PLATFORM_ONLY_LIBS) but its module keeps the ELF check"
    fi
    have=$(declared "$p")
    [ "$want" = "$have" ] || fail "bin/$p needs [$(echo $want)] but Android.bp declares [$(echo $have)]"
done

{
    echo "x-kernel VM package, binary delivery"
    echo "created:          $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "Android baseline: $baseline"
    echo "t11206-pvm-service: $service_commit"
    echo "t11206-pvm-app:     $app_commit"
    echo "programs and app from: $product_out"
    echo "prebuilts from:        $prebuilts"
    echo
    echo "Shared libraries of the programs (DT_NEEDED):"
    for p in "${programs[@]}"; do echo "  $p: $(needed "$out/bin/$p" | tr '\n' ' ')"; done
    echo "  ($PLATFORM_ONLY_LIBS: provided by the platform image, /system/lib64; not declared, see Android.bp)"
    echo
    echo "sha256 of every file:"
    (cd "$out" && find . -type f ! -name MANIFEST.txt | sed 's|^\./||' | sort | while read -r f; do echo "  $(sha "$f")  $f"; done)
} > "$out/MANIFEST.txt"
echo "make-drop: wrote $out ($(find "$out" -type f | wc -l | tr -d ' ') files); see $out/MANIFEST.txt"
