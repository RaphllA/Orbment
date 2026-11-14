#Requires -Version 7.0
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
$buildDir = Join-Path $root 'target'
. (Join-Path $PSScriptRoot 'dependency-notices.ps1')

Push-Location -LiteralPath $root
try {
    $metadata = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo workspace metadata.' }
    $version = ($metadata | ConvertFrom-Json).packages.Where({ $_.name -eq 'orbment-app' })[0].version

    cargo build --locked --release --target $target --target-dir $buildDir -p orbment-app
    if ($LASTEXITCODE -ne 0) { throw 'Orbment release build failed.' }

    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    $archivePath = Join-Path $dist "Orbment-v$version-windows-x64-portable.zip"
    $tempPath = Join-Path $dist "$([guid]::NewGuid()).zip.tmp"
    $dependencyNotices = New-TemporaryFile
    try {
        Write-OrbmentDependencyNotices -Target $target -OutputPath $dependencyNotices.FullName
        $sysroot = rustc --print sysroot
        if ($LASTEXITCODE -ne 0) { throw 'Could not locate the Rust toolchain notices.' }
        $rustNotice = Join-Path $sysroot 'share/doc/rust/COPYRIGHT-library.html'
        if (!(Test-Path -LiteralPath $rustNotice -PathType Leaf)) {
            throw 'Rust standard library notices are missing. Install the rust-docs component for this toolchain.'
        }
        $zip = [IO.Compression.ZipFile]::Open($tempPath, [IO.Compression.ZipArchiveMode]::Create)
        try {
            $files = [ordered]@{
                'orbment.exe' = (Join-Path $buildDir "$target/release/orbment.exe")
                'assets/locales/zh-CN.toml' = (Join-Path $root 'assets/locales/zh-CN.toml')
                'README.md' = (Join-Path $root 'README.md')
                'LICENSE' = (Join-Path $root 'LICENSE')
                'THIRD-PARTY-NOTICES.txt' = (Join-Path $root 'THIRD-PARTY-NOTICES.txt')
                'licenses/Apache-2.0.txt' = (Join-Path $root 'licenses/Apache-2.0.txt')
                'licenses/DEPENDENCIES.txt' = $dependencyNotices.FullName
                'licenses/RUST-STANDARD-LIBRARY.html' = $rustNotice
            }
            foreach ($entry in $files.GetEnumerator()) {
                [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $entry.Value, $entry.Key) | Out-Null
            }
            $zip.CreateEntry('portable.flag') | Out-Null
        } finally {
            $zip.Dispose()
        }
        [IO.File]::Move($tempPath, $archivePath, $true)
    } finally {
        if (Test-Path -LiteralPath $tempPath) { Remove-Item -LiteralPath $tempPath }
        Remove-Item -LiteralPath $dependencyNotices.FullName
    }
    Write-Output $archivePath
} finally {
    Pop-Location
}
