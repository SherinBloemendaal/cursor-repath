# Install crepath from GitHub releases.
# https://github.com/SherinBloemendaal/cursor-repath
#Requires -Version 5.1

$ErrorActionPreference = 'Stop'

function Write-CrepathLine {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host $Message
}

function Write-Crepath {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param(
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string]$Message,
        [Parameter(Mandatory = $true)][string]$Color
    )
    Write-Host -NoNewline -ForegroundColor $Color -Object "$Label "
    Write-Host $Message
}

function Get-CrepathVersion {
    param([string]$Raw)
    if ([string]::IsNullOrWhiteSpace($Raw)) {
        throw 'empty version'
    }
    if ($Raw -notmatch '^[A-Za-z0-9._-]+$') {
        throw "invalid version: $Raw"
    }
    if ($Raw.StartsWith('v')) {
        return $Raw
    }
    return "v$Raw"
}

function Save-CrepathFile {
    param(
        [Parameter(Mandatory = $true)][string]$Url,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($null -ne $curl) {
        & curl.exe -fL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 --output $Destination $Url
        if ($LASTEXITCODE -ne 0) {
            throw "download failed: $Url"
        }
        return
    }
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
}

function Get-CrepathChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$SumsPath,
        [Parameter(Mandatory = $true)][string]$Asset
    )
    foreach ($line in (Get-Content -Path $SumsPath)) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(\S+)\s*$') {
            if ($Matches[2] -eq $Asset) {
                return $Matches[1].ToLowerInvariant()
            }
        }
    }
    throw "checksum mismatch: SHA256SUMS has no entry for $Asset"
}

try {
    if ($env:OS -ne 'Windows_NT') {
        throw 'unsupported platform: install.ps1 is for Windows. Use install.sh on macOS and Linux.'
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if (-not [string]::IsNullOrEmpty($env:PROCESSOR_ARCHITEW6432)) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    if ($arch -ne 'AMD64') {
        throw "unsupported platform: windows $arch. crepath publishes windows x64 (x86_64-pc-windows-msvc)."
    }

    $repo = 'SherinBloemendaal/cursor-repath'
    $github = "https://github.com/$repo"
    $target = 'x86_64-pc-windows-msvc'
    $asset = "crepath-$target.zip"

    if (-not [string]::IsNullOrWhiteSpace($env:CREPATH_VERSION)) {
        $version = Get-CrepathVersion $env:CREPATH_VERSION
    }
    else {
        $latestUrl = "$github/releases/latest"
        $effective = & curl.exe -fsSL --proto '=https' --tlsv1.2 -o NUL -w '%{url_effective}' $latestUrl
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($effective)) {
            throw "could not find the latest release at $latestUrl"
        }
        if ($effective -notmatch '/releases/tag/([^/?#]+)$') {
            throw "unexpected latest-release URL: $effective"
        }
        $version = Get-CrepathVersion $Matches[1]
    }

    if ([string]::IsNullOrWhiteSpace($env:CREPATH_INSTALL)) {
        $installDir = Join-Path $env:USERPROFILE '.crepath\bin'
    }
    else {
        $installDir = $env:CREPATH_INSTALL.TrimEnd('\')
    }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $installDir = (Resolve-Path -Path $installDir).Path

    Write-Crepath -Label 'info' -Color 'Cyan' -Message "installing crepath $version ($target)"

    $tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("crepath-install-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
    try {
        $archive = Join-Path $tempRoot $asset
        $sums = Join-Path $tempRoot 'SHA256SUMS'
        $base = "$github/releases/download/$version"
        Save-CrepathFile -Url "$base/$asset" -Destination $archive
        Save-CrepathFile -Url "$base/SHA256SUMS" -Destination $sums

        $expected = Get-CrepathChecksum -SumsPath $sums -Asset $asset
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "checksum mismatch for ${asset}: expected $expected actual $actual"
        }
        Write-Crepath -Label 'ok' -Color 'Green' -Message 'checksum verified'

        $extract = Join-Path $tempRoot 'extract'
        New-Item -ItemType Directory -Force -Path $extract | Out-Null
        Expand-Archive -Path $archive -DestinationPath $extract -Force
        $exe = Get-ChildItem -Path $extract -Filter 'crepath.exe' -Recurse -File | Select-Object -First 1
        if ($null -eq $exe) {
            throw 'archive does not contain crepath.exe'
        }

        $dest = Join-Path $installDir 'crepath.exe'
        $stage = Join-Path $installDir 'crepath.exe.new'
        Copy-Item -Force -Path $exe.FullName -Destination $stage
        if (Test-Path -Path $dest) {
            Remove-Item -Force -Path $dest
        }
        Move-Item -Force -Path $stage -Destination $dest
        Write-Crepath -Label 'ok' -Color 'Green' -Message "installed $dest"
    }
    finally {
        if (Test-Path -Path $tempRoot) {
            Remove-Item -Recurse -Force -Path $tempRoot
        }
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $needle = $installDir.TrimEnd('\').ToLowerInvariant()
    $present = $false
    if (-not [string]::IsNullOrEmpty($userPath)) {
        foreach ($part in ($userPath -split ';')) {
            $item = $part.Trim().TrimEnd('\').ToLowerInvariant()
            if ($item -eq $needle) {
                $present = $true
            }
        }
    }
    if (-not $present) {
        if ([string]::IsNullOrEmpty($userPath)) {
            $newPath = $installDir
        }
        else {
            $newPath = "$installDir;$userPath"
        }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Crepath -Label 'ok' -Color 'Green' -Message "added $installDir to the user PATH"
        Write-Crepath -Label 'info' -Color 'Cyan' -Message 'open a new terminal so PATH updates, then run crepath'
    }
    else {
        Write-Crepath -Label 'info' -Color 'Cyan' -Message "PATH already includes $installDir"
    }

    if ([string]::IsNullOrEmpty($env:Path) -or ($env:Path.ToLowerInvariant() -notlike "*$needle*")) {
        $env:Path = "$installDir;$env:Path"
    }

    $printed = & $dest --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-CrepathLine -Message "$printed"
    }
    else {
        Write-Crepath -Label 'info' -Color 'Cyan' -Message "crepath $version"
    }
    Write-Crepath -Label 'ok' -Color 'Green' -Message 'run crepath'
}
catch {
    Write-Crepath -Label 'error' -Color 'Red' -Message $_.Exception.Message
    exit 1
}
