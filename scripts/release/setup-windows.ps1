# Run after ilammy/msvc-dev-cmd with architecture x64.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

function Get-VerifiedFile($Url, $Path, $Sha256) {
    Invoke-WebRequest -Uri $Url -OutFile $Path
    if ((Get-FileHash -Algorithm SHA256 $Path).Hash -ne $Sha256) {
        throw "SHA-256 mismatch for $Url"
    }
}

$fxc = Join-Path $env:WindowsSdkVerBinPath 'x64/fxc.exe'
if (!(Test-Path -LiteralPath $fxc -PathType Leaf)) { throw 'Windows SDK x64 fxc.exe not found' }

$root = Join-Path $env:RUNNER_TEMP 'ghostty-vt-msvc'
New-Item -ItemType Directory -Path $root | Out-Null
Push-Location $root
try {
    $zigName = 'zig-x86_64-windows-0.17.1'
    Get-VerifiedFile "https://ziglang.org/download/0.17.1/$zigName.zip" zig.zip b03f477a1943673e02aca0f570e8030dffd536ce1f45cbc4d3326ec31159562e
    Expand-Archive zig.zip -DestinationPath .
    $zig = Join-Path $root "$zigName/zig.exe"

    $release = 'https://release.files.ghostty.org/libghostty-vt/1.0.1'
    Get-VerifiedFile "$release/libghostty-vt-1.0.1.tar.gz" vt.tar.gz 333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa
    Get-VerifiedFile "$release/libghostty-vt-1.0.1-deps.tar.gz" deps.tar.gz 91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d
    New-Item -ItemType Directory -Path source, deps | Out-Null
    tar -xzf vt.tar.gz --strip-components 1 -C source
    tar -xzf deps.tar.gz -C deps
    $deps = Join-Path $root 'deps'
    foreach ($candidate in @('p', 'libghostty-vt-1.0.1-deps/p', 'libghostty-vt-1.0.1-deps')) {
        $path = Join-Path $deps $candidate
        if (Test-Path -LiteralPath $path -PathType Container) { $deps = $path; break }
    }
    $out = Join-Path $root 'out'
    Set-Location source
    & $zig build '-Dtarget=x86_64-windows-msvc' '-Demit-lib-vt' '-Demit-exe=false' '-Doptimize=ReleaseFast' '-Dcpu=baseline' --system $deps --prefix $out
    $lib = Join-Path $out 'lib'
    if (!(Test-Path -LiteralPath (Join-Path $lib 'ghostty-vt-static.lib') -PathType Leaf)) {
        throw 'Zig did not produce ghostty-vt-static.lib'
    }
    "GPUI_FXC_PATH=$fxc", "GHOSTTY_VT_LIB_DIR=$lib" | Add-Content -Encoding utf8 -LiteralPath $env:GITHUB_ENV
} finally {
    Pop-Location
}
