#!/bin/bash
# Cross-builds amd_fidelityfx_dx12.dll (FSR 3.1 upscaler + FSR 3 frame
# generation) from AMD's MIT-licensed FidelityFX SDK v1.1.4 for x64 Windows,
# on Linux: clang-cl / lld-link / llvm-rc and the Windows SDK from
# cargo-xwin, the SDK's own shader compiler (FidelityFX_SC.exe) under wine.
# Used on the build farm; xtask/build-ffx-dll.ps1 is the MSVC route.
#
#   bash xtask/ffx-cross-build.sh <sdk source copy> <build dir>
#
# The source copy is patched in place (Linux and clang-cl only, no change to
# what is compiled):
# - the shader rule's output list kept the list's own name as a dependency,
#   which MSBuild ignores and Ninja refuses;
# - include paths spelled in another case (FidelityFx/, ffx_FrameInterpolation.h);
# - warnings as errors off (clang-cl warns where MSVC does not);
# - the DLL's version resource dropped (it needs MFC's winres.h);
# - Dwmapi.lib linked as the xwin SDK spells it;
# - the shader compiler runs under wine with Z: paths.
set -euo pipefail
SRC=$1
B=$2
X=$HOME/.cache/cargo-xwin/xwin
TOOLS=$(mktemp -d)
test "$(cat "$SRC/.ffx-commit" 2>/dev/null || git -C "$SRC" rev-parse HEAD)" = c6efa6bf7f2027b3ec94f28578bb5965eabb9e55

cat > "$TOOLS/ffx-sc" <<W
#!/usr/bin/env python3
import os, re, sys, subprocess
SC = "$SRC/sdk/tools/binary_store/FidelityFX_SC.exe"
W
cat >> "$TOOLS/ffx-sc" <<'W'
def conv(a):
    return re.sub(r"(^|=|-I |-I)(/[^ ]*)", lambda m: m.group(1) + "Z:" + m.group(2).replace("/", "\\"), a)
env = dict(os.environ, WINEDEBUG="-all")
r = subprocess.run(["wine", SC] + [conv(a) for a in sys.argv[1:]], env=env,
                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
sys.stdout.write(r.stdout)
out = next((a.split("=", 1)[1] for a in sys.argv[1:] if a.startswith("-output=")), None)
if out:
    for f in os.listdir(out):
        if f.endswith(".d"):
            p = os.path.join(out, f)
            s = open(p, errors="replace").read()
            t = re.sub(r"[Zz]:\\\\?([^ \n]*)", lambda m: "/" + m.group(1).replace("\\\\", "/").replace("\\", "/"), s)
            if t != s:
                open(p, "w").write(t)
sys.exit(r.returncode)
W
chmod +x "$TOOLS/ffx-sc"

python3 - "$SRC" "$TOOLS/ffx-sc" <<'P'
import sys, os
src, sc = sys.argv[1], sys.argv[2]
def edit(rel, a, b):
    p = os.path.join(src, rel)
    s = open(p, newline="").read()
    if b in s:
        return
    assert a in s, (rel, a)
    open(p, "w", newline="").write(s.replace(a, b, 1))
p = "sdk/include/FidelityFX/gpu/CMakeCompileShaders.txt"
s = open(os.path.join(src, p), newline="").read()
nl = "\r\n" if "\r\n" in s else "\n"
edit(p, "\t#message(STATUS \"OUTPUT_PATH=${OUTPUT_PATH}\")",
     "\tset(_FFX_OUT_NAME ${PERMUTATION_OUTPUTS})" + nl + "\tset(PERMUTATION_OUTPUTS )" + nl + "\t#message(STATUS \"OUTPUT_PATH=${OUTPUT_PATH}\")")
edit(p, "\tset(${PERMUTATION_OUTPUTS} ${PERMUTATION_OUTPUTS} PARENT_SCOPE)",
     "\tset(${_FFX_OUT_NAME} ${PERMUTATION_OUTPUTS} PARENT_SCOPE)")
for rel in ["sdk/src/backends/dx12/CMakeLists.txt", "sdk/src/components/fsr1/CMakeLists.txt", "sdk/src/components/fsr2/CMakeLists.txt"]:
    q = os.path.join(src, rel)
    t = open(q, newline="").read()
    open(q, "w", newline="").write(t.replace('"/WX"', '""'))
q = os.path.join(src, "sdk/CMakeLists.txt")
t = open(q, newline="").read()
import re
t = re.sub(r"set\(FFX_SC_EXECUTABLE [^)]*\)", "set(FFX_SC_EXECUTABLE " + sc + ")", t)
open(q, "w", newline="").write(t)
open(os.path.join(src, "ffx-api/src/resource/ffx_api_dll.rc"), "w").write("// version resource dropped for the cross build\n")
P
# A `#pragma comment(lib, "Dwmapi.lib")`; the xwin SDK names it in lower case.
ln -sf dwmapi.lib "$X/sdk/lib/um/x86_64/Dwmapi.lib"
ln -sfn FidelityFX "$SRC/sdk/include/FidelityFx"
ln -sf ffx_frameinterpolation.h "$SRC/sdk/include/FidelityFX/host/ffx_FrameInterpolation.h"

export PATH=$HOME/scarlettport-farm/bin:$PATH WINEDEBUG=-all
rm -rf "$B"
mkdir -p "$B"
nice -n 15 cmake -S "$SRC/ffx-api" -B "$B" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_TOOLCHAIN_FILE=$HOME/.cache/cargo-xwin/cmake/clang-cl/x86_64-pc-windows-msvc-toolchain.cmake \
  -DCMAKE_EXE_LINKER_FLAGS=/machine:x64 -DFFX_API_BACKEND=DX12_X64 \
  "-DCMAKE_CXX_FLAGS=-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH -Wno-everything" \
  "-DCMAKE_C_FLAGS=-Wno-everything" > "$B/config.log" 2>&1
nice -n 15 ninja -C "$B" -j 32 > "$B/build.log" 2>&1 || { grep -E "error" "$B/build.log" | head -20; exit 1; }
ls -la "$SRC/ffx-api/bin/amd_fidelityfx_dx12.dll"
sha256sum "$SRC/ffx-api/bin/amd_fidelityfx_dx12.dll"
