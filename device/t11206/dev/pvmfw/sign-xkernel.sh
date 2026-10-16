#!/usr/bin/env bash
# Signs an x-kernel image with the vendor's SM2 test key so the vendor pvmfw accepts it
# (AVB hash footer, SM3_SM2). Runs in the aarch64 Linux VM (`ssh orb`), which sees the Mac's
# /Volumes paths unchanged and can run the existing aarch64 gmssl build.
#
#   sign-xkernel.sh UNSIGNED OUT
#
# Taken from the signer proven on N80 on 2026-09-29 (record n80-20260929-avf-r52, stage 3);
# only the paths changed. Its two constraints stay:
#  * a fixed salt: avbtool picks a random one by default, which changes the digest pvmfw feeds
#    into DICE, so an existing instance.img could no longer be opened;
#  * the SM2 signature DER must fill the 72-byte slot exactly (it comes out 70..72 bytes); the
#    deployed pvmfw only accepted that shape.
set -Eeuo pipefail

IN=$1; OUT=$2
ST=${SIGN_TOOLS:-/Volumes/Dev/Code/kylin100-xkernel-boots/sign-tools}
G=${GMSSL_BUILD:-/Volumes/Dev/Code/GmSSL/build}
SALT=dc20fcae07af2252d4e1bcfff57de63f458c1a0492b17d2d8c78ced5a61c01b1
export PATH="$G/bin:$PATH" LD_LIBRARY_PATH="$G/bin:$G/lib"
command -v gmssl >/dev/null || { echo "gmssl not in $G/bin"; exit 1; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
cp "$IN" "$work/kernel.bin"

python3 "$ST/avbtool.py" add_hash_footer --image "$work/kernel.bin" \
  --partition_name boot --algorithm SM3_SM2 --dynamic_partition_size \
  --key "$ST/sm2.pem" --pubkey "$ST/sm2pub.pem" --hash_algorithm sm3 --salt "$SALT"

# The AVBf footer (last 64 bytes) gives the vbmeta offset.
VBOFF=$(python3 - "$work/kernel.bin" <<'PY'
import struct, sys
b = open(sys.argv[1], 'rb').read(); f = b[-64:]
assert f[:4] == b'AVBf', "no AVBf footer"
print(struct.unpack('>Q', f[20:28])[0])
PY
)
python3 - "$work/kernel.bin" "$VBOFF" "$work/hash.bin" <<'PY'
import sys
b = open(sys.argv[1], 'rb').read(); o = int(sys.argv[2])
open(sys.argv[3], 'wb').write(b[o + 256:o + 288])   # the auth block's hash field
PY

for i in $(seq 1 200); do
  gmssl sm2sign -key "$ST/sm2.pem" -pass 1234 -in "$work/hash.bin" -out "$work/sig.der"
  n=$(stat -c %s "$work/sig.der")
  [ "$n" -eq 72 ] && break
done
[ "$n" -eq 72 ] || { echo "no 72-byte DER in 200 attempts"; exit 1; }
echo "72-byte signature DER on attempt $i"

python3 - "$work/kernel.bin" "$VBOFF" "$work/sig.der" "$OUT" <<'PY'
import sys
b = bytearray(open(sys.argv[1], 'rb').read()); o = int(sys.argv[2])
sig = open(sys.argv[3], 'rb').read(); assert len(sig) == 72
b[o + 288:o + 360] = sig                             # the auth block's signature slot
open(sys.argv[4], 'wb').write(bytes(b))
PY

# Self-check: verify the signature written into OUT with the public key.
python3 - "$OUT" "$VBOFF" "$work" <<'PY'
import sys
b = open(sys.argv[1], 'rb').read(); o = int(sys.argv[2]); w = sys.argv[3]
open(f"{w}/v_hash.bin", 'wb').write(b[o + 256:o + 288])
open(f"{w}/v_sig.der", 'wb').write(b[o + 288:o + 360])
PY
cmp "$work/v_hash.bin" "$work/hash.bin"
gmssl sm2verify -pubkey "$ST/sm2pub.pem" -sig "$work/v_sig.der" -in "$work/v_hash.bin"
python3 "$ST/avbtool.py" info_image --image "$OUT" | grep -E "Algorithm|Public key|Digest|Salt"
echo "out: $OUT ($(stat -c %s "$OUT") bytes) sha256 $(sha256sum "$OUT" | cut -c1-16)"
