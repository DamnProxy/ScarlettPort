# Builds amd_fidelityfx_dx12.dll (FSR 3.1 upscaler + FSR 3 frame generation)
# from AMD's MIT-licensed FidelityFX SDK v1.1.4 source, for victory's
# VICTORY_UPSCALER=fsr and VICTORY_FRAMEGEN=1 (crates/victory-gpu/src/fsr.rs).
# The DLL is not committed; copy it next to victory.exe.
#
#   .\xtask\build-ffx-dll.ps1 -Src <ffx-sdk-source> -Out <target>\release
#
# Needs Visual Studio 2022 (C++ desktop, Windows SDK) and CMake. Runs at low
# priority. The source must be tag v1.1.4 (commit c6efa6bf7f2027b3ec94f28578bb5965eabb9e55):
#   git clone --depth 1 --branch v1.1.4 https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK.git ffx-src
param(
    [Parameter(Mandatory)][string]$Src,
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
$commit = git -C $Src rev-parse HEAD
if ($commit -ne 'c6efa6bf7f2027b3ec94f28578bb5965eabb9e55') { throw "expected FidelityFX SDK v1.1.4, found $commit" }
$build = Join-Path $Src 'ffx-api\build'
New-Item -ItemType Directory -Force $build | Out-Null
$args1 = "-S `"$Src\ffx-api`" -B `"$build`" -G `"Visual Studio 17 2022`" -A x64 -DFFX_API_BACKEND=DX12_X64"
$args2 = "--build `"$build`" --config Release --parallel 4"
foreach ($a in @($args1, $args2)) {
    $p = Start-Process cmake -ArgumentList $a -NoNewWindow -PassThru -Wait:$false
    $p.PriorityClass = 'BelowNormal'
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "cmake $a failed ($($p.ExitCode))" }
}
$dll = Get-ChildItem -Recurse -Path $Src -Filter amd_fidelityfx_dx12.dll |
    Where-Object { $_.FullName -notmatch 'PrebuiltSignedDLL' } |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $dll) { throw 'no amd_fidelityfx_dx12.dll was built' }
Copy-Item $dll.FullName $Out -Force
$hash = (Get-FileHash (Join-Path $Out 'amd_fidelityfx_dx12.dll') -Algorithm SHA256).Hash
Write-Host "built $($dll.FullName)"
Write-Host "SHA-256 $hash"
