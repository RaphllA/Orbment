function Write-OrbmentDependencyNotices {
    param(
        [Parameter(Mandatory)] [string] $Target,
        [Parameter(Mandatory)] [string] $OutputPath
    )

    $metadata = cargo metadata --locked --filter-platform $Target --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Could not resolve Cargo dependencies for license notices.' }
    $graph = $metadata | ConvertFrom-Json
    $packages = @{}
    foreach ($package in $graph.packages) { $packages[$package.id] = $package }
    $nodes = @{}
    foreach ($node in $graph.resolve.nodes) { $nodes[$node.id] = $node }

    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push(($graph.packages | Where-Object name -eq 'orbment-app').id)
    $seen = [Collections.Generic.HashSet[string]]::new()
    $dependencies = while ($pending.Count) {
        $id = $pending.Pop()
        if (!$seen.Add($id)) { continue }
        foreach ($dependency in $nodes[$id].deps) {
            if ($null -in $dependency.dep_kinds.kind -or 'build' -in $dependency.dep_kinds.kind) {
                $pending.Push($dependency.pkg)
            }
        }
        if ($packages[$id].source) { $packages[$id] }
    }

    # These crate archives omit the license files at the upstream workspace root.
    # The copies in licenses/ come from the revisions recorded in .cargo_vcs_info.json.
    $fallback = @{
        'ecolor@0.31.1' = 'egui'
        'egui@0.31.1' = 'egui'
        'emath@0.31.1' = 'egui'
        'epaint@0.31.1' = 'egui'
        'epaint_default_fonts@0.31.1' = 'egui'
        'profiling@1.0.18' = 'profiling'
    }
    $licenses = Join-Path (Split-Path -Parent $PSScriptRoot) 'licenses'
    $textIds = [Collections.Generic.Dictionary[string, int]]::new([StringComparer]::Ordinal)
    $texts = [Collections.Generic.List[string]]::new()
    $writer = [IO.StreamWriter]::new($OutputPath, $false, [Text.UTF8Encoding]::new($false))
    try {
        $writer.WriteLine("Orbment $Target Cargo dependency notices")
        $writer.WriteLine('Includes runtime and build dependencies for orbment-app; excludes test-only dependencies.')
        $writer.WriteLine('Each license or notice text is reproduced once and referenced by number.')
        foreach ($package in $dependencies | Sort-Object name, version) {
            $key = "$($package.name)@$($package.version)"
            $dir = Split-Path -Parent $package.manifest_path
            $files = @(Get-ChildItem -LiteralPath $dir -File -Recurse | Where-Object {
                $_.Name -match '^(licen[cs]e|copying|notice|unlicense|copyright|ofl|ufl)([._-]|$)' -and
                    !($package.name -eq 'libsqlite3-sys' -and $_.Directory.Name -eq 'sqlcipher')
            } | Sort-Object FullName)
            if ($package.license_file) { $files += Get-Item -LiteralPath (Join-Path $dir $package.license_file) }
            if ($fallback.ContainsKey($key)) {
                foreach ($suffix in 'MIT', 'APACHE') {
                    $files += Get-Item -LiteralPath (Join-Path $licenses "$($fallback[$key])-LICENSE-$suffix.txt")
                }
            }
            if ($package.name -eq 'epaint_default_fonts') {
                $files += Get-Item -LiteralPath (Join-Path $dir 'fonts/Hack-Regular.txt'), (Join-Path $dir 'fonts/emoji-icon-font-mit-license.txt')
            }
            if (!$files.Count) { throw "No license text found for $key." }

            $writer.WriteLine()
            $writer.WriteLine("$($package.name) $($package.version): $($package.license)")
            if ($package.repository) { $writer.WriteLine($package.repository) }
            foreach ($file in $files | Sort-Object FullName -Unique) {
                $body = [IO.File]::ReadAllText($file.FullName)
                if (!$textIds.ContainsKey($body)) {
                    $texts.Add($body)
                    $textIds.Add($body, $texts.Count)
                }
                $name = if ($file.FullName.StartsWith($dir + [IO.Path]::DirectorySeparatorChar)) {
                    [IO.Path]::GetRelativePath($dir, $file.FullName).Replace('\', '/')
                } else {
                    $file.Name
                }
                $writer.WriteLine("  $name [text $($textIds[$body])]")
            }
        }
        for ($i = 0; $i -lt $texts.Count; $i++) {
            $writer.WriteLine()
            $writer.WriteLine("========== text $($i + 1) ==========")
            $writer.WriteLine($texts[$i])
        }
    } finally {
        $writer.Dispose()
    }
}
