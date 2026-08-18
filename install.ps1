# TerminalReader installer for Windows: downloads the latest release and
# installs it to $env:LOCALAPPDATA\Programs\TerminalReader (override with
# $env:TR_INSTALL_DIR), then adds that directory to the user PATH.
#
#   irm https://raw.githubusercontent.com/Kardzhilov/TerminalReader/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$Repo = "Kardzhilov/TerminalReader"
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:TR_INSTALL_DIR) { $env:TR_INSTALL_DIR } else {
    Join-Path $env:LOCALAPPDATA "Programs\TerminalReader"
}

if ([System.Environment]::OSVersion.Platform -ne "Win32NT") {
    throw "This script is for Windows. On Linux/macOS use install.sh instead."
}
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne "X64") {
    throw "Unsupported architecture: $arch (releases are built for x86_64 only)."
}

Write-Host "Looking up the latest release of $Repo..."
$release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
$tag = $release.tag_name
if (-not $tag) { throw "Could not determine the latest release tag." }

$asset = "terminalreader-$tag-$Target.zip"
$url = "https://github.com/$Repo/releases/download/$tag/$asset"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "tr-install-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $asset ($tag)..."
    $zip = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $url -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $tmp

    $binary = Join-Path $tmp "terminalreader-$tag-$Target\terminalreader.exe"
    if (-not (Test-Path $binary)) { throw "Archive did not contain terminalreader.exe." }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item $binary (Join-Path $InstallDir "terminalreader.exe") -Force
    Write-Host "Installed terminalreader $tag to $InstallDir"

    # Persist on the user PATH so new terminals can find it.
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
        Write-Host "Added $InstallDir to your user PATH. Open a new terminal, then run 'terminalreader'."
    }
    else {
        Write-Host "Run 'terminalreader' to get started."
    }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
