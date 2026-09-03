# Download the official LGPL LibVLC distribution; do not use a VLC installation
# or download executable code while a user is previewing a file.
param([string]$PackagePath, [string]$Proxy)
$ErrorActionPreference = 'Stop'
$version = '3.0.23.1'
$expectedHash = '70927AFA9AD34B77E7D9A5E6D02CAE099771F6EB3114DA18111A4B76F65B836F'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$target = [IO.Path]::GetFullPath((Join-Path $repo 'target'))
New-Item -ItemType Directory -Path $target -Force | Out-Null
if (-not $PackagePath) {
    $PackagePath = Join-Path $target "videolan.libvlc.windows.$version.nupkg"
    if (-not (Test-Path -LiteralPath $PackagePath)) {
        $download = @{
            Uri = "https://api.nuget.org/v3-flatcontainer/videolan.libvlc.windows/$version/videolan.libvlc.windows.$version.nupkg"
            OutFile = $PackagePath
        }
        if ($Proxy) { $download.Proxy = $Proxy }
        Invoke-WebRequest @download
    }
}
if ((Get-FileHash -LiteralPath $PackagePath -Algorithm SHA256).Hash -ne $expectedHash) {
    throw 'LibVLC package checksum mismatch; no runtime files have been installed.'
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $PackagePath))
try {
    $dll = @($zip.Entries | Where-Object { $_.FullName -match '(^|/)x64/libvlc\.dll$' })
    if ($dll.Count -ne 1) { throw 'Unexpected LibVLC package layout.' }
    $prefix = $dll[0].FullName.Substring(0, $dll[0].FullName.Length - 'libvlc.dll'.Length)
    foreach ($profile in @('debug', 'release')) {
        $destination = [IO.Path]::GetFullPath((Join-Path $target "$profile/libvlc"))
        if (-not $destination.StartsWith($target + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Runtime destination is outside the build directory.'
        }
        if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Recurse -Force }
        New-Item -ItemType Directory -Path $destination -Force | Out-Null
        foreach ($entry in $zip.Entries) {
            if (-not $entry.FullName.StartsWith($prefix) -or $entry.FullName.EndsWith('/')) { continue }
            $relative = $entry.FullName.Substring($prefix.Length)
            $path = [IO.Path]::GetFullPath((Join-Path $destination $relative))
            if (-not $path.StartsWith($destination + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'Invalid runtime archive path.'
            }
            New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($path)) -Force | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $path, $true)
        }
        Copy-Item -LiteralPath (Join-Path $repo 'THIRD_PARTY_LICENSES.md') -Destination $destination
        Copy-Item -LiteralPath (Join-Path $repo 'assets/licenses/COPYING-LibVLC.txt') -Destination $destination
        Write-Output "Prepared verified LibVLC $version in $destination"
    }
} finally { $zip.Dispose() }
