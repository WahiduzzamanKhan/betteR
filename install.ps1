<#
.SYNOPSIS
Builds better from source on Windows and installs it with cargo.

.DESCRIPTION
Run it from the betteR folder:

    powershell -ExecutionPolicy Bypass -File install.ps1

It installs what's missing (the Visual Studio C++ build tools, and Rust through
rustup), asking first, then runs `cargo install --path . --locked`. better is
built for the architecture of your R, so it can load R's DLLs. On Windows on
ARM it also sets up what the `ring` crate needs there: Clang, and the ARM64
compiler environment that lets Clang find the Windows headers.

Installing the build tools needs administrator rights; Windows asks once.

.PARAMETER Yes
Answer yes to every question (for unattended installs).
#>
param(
    [switch]$Yes
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell's progress bar makes downloads many times slower.
$ProgressPreference = 'SilentlyContinue'

function Write-Step($text) { Write-Host "==> $text" -ForegroundColor Cyan }

function Confirm-Step($question) {
    if ($Yes) { return $true }
    $answer = Read-Host "$question [Y/n]"
    return ($answer -eq '' -or $answer -match '^[yY]')
}

function Invoke-Native {
    param([string]$File, [string[]]$Arguments)
    # With 'Stop', Windows PowerShell can treat a native program's stderr as an error.
    $ErrorActionPreference = 'Continue'
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$File $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

# The machine type in a DLL's PE header: 'arm64', 'x64' or $null.
function Get-DllArch($path) {
    $stream = [IO.File]::OpenRead($path)
    try {
        $reader = New-Object IO.BinaryReader($stream)
        $stream.Position = 0x3C
        $stream.Position = $reader.ReadInt32() + 4
        switch ($reader.ReadUInt16()) {
            0xAA64 { return 'arm64' }
            0x8664 { return 'x64' }
            default { return $null }
        }
    } finally {
        $stream.Dispose()
    }
}

# How better will find R: $R_HOME, else `R RHOME` from PATH (as ark does), else
# only the registry, which R's installer always writes but ark doesn't read.
function Find-R {
    $ErrorActionPreference = 'Continue'
    if ($env:R_HOME -and (Test-Path $env:R_HOME)) {
        return @{ Home = $env:R_HOME; Source = 'R_HOME' }
    }
    $r = Get-Command R.exe -ErrorAction SilentlyContinue
    if ($r) {
        $rhome = (& $r.Source RHOME 2>$null | Select-Object -Last 1)
        if ($LASTEXITCODE -eq 0 -and $rhome -and (Test-Path $rhome)) {
            return @{ Home = $rhome.Trim(); Source = 'PATH' }
        }
    }
    foreach ($key in 'HKCU:\SOFTWARE\R-core\R', 'HKLM:\SOFTWARE\R-core\R') {
        $installPath = (Get-ItemProperty $key -ErrorAction SilentlyContinue).InstallPath
        if ($installPath -and (Test-Path $installPath)) {
            return @{ Home = $installPath; Source = 'registry' }
        }
    }
    return $null
}

$root = $PSScriptRoot
if (-not (Test-Path (Join-Path $root 'Cargo.toml'))) { throw "Run install.ps1 from the betteR folder." }

# --- Which architecture to build for ----------------------------------------
# The native architecture, even when this PowerShell runs under emulation.
$osArch = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
$osArch = switch ($osArch) { 'ARM64' { 'arm64' } 'AMD64' { 'x64' } default { throw "Unsupported Windows architecture: $osArch" } }

Write-Step 'Looking for R'
$r = Find-R
$arch = $osArch
if ($r) {
    $dll = Get-ChildItem (Join-Path $r.Home 'bin') -Recurse -Filter R.dll -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($dll) {
        $rArch = Get-DllArch $dll.FullName
        if ($rArch) { $arch = $rArch }
    }
    Write-Host "R ($arch) at $($r.Home), found through $($r.Source)."
} else {
    Write-Warning "R not found. Building for $arch; install R for $arch (https://cran.r-project.org) before running better."
}

$target = if ($arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$vsArch = if ($arch -eq 'arm64') { 'arm64' } else { 'amd64' }
$hostArch = if ($osArch -eq 'arm64') { 'arm64' } else { 'amd64' }

# --- Visual Studio C++ build tools -------------------------------------------
$vcTools = if ($arch -eq 'arm64') { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
$clangComponent = 'Microsoft.VisualStudio.Component.VC.Llvm.Clang'
$installerDir = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer'
$vswhere = Join-Path $installerDir 'vswhere.exe'

function Find-VisualStudio {
    if (-not (Test-Path $vswhere)) { return $null }
    $json = & $vswhere -latest -products * -format json -include packages
    # ForEach-Object unrolls the array, which Windows PowerShell's ConvertFrom-Json doesn't.
    $instances = @($json | Out-String | ConvertFrom-Json | ForEach-Object { $_ })
    if ($instances.Count -eq 0) { return $null }
    $vs = $instances[0]
    $packages = @($vs.packages | ForEach-Object { $_.id })
    return @{
        Path = $vs.installationPath
        Product = $vs.productId
        Has = { param($id) $packages -contains $id }.GetNewClosure()
    }
}

# ring builds its ARM64 assembly with Clang, from Visual Studio or from LLVM.
function Find-Clang($vsPath) {
    $candidates = @()
    if ($vsPath) { $candidates += Join-Path $vsPath 'VC\Tools\Llvm\ARM64\bin\clang.exe' }
    $onPath = Get-Command clang.exe -ErrorAction SilentlyContinue
    if ($onPath) { $candidates += $onPath.Source }
    $candidates += Join-Path $env:ProgramFiles 'LLVM\bin\clang.exe'
    return $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}

function Test-WindowsSdk {
    $include = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Include'
    return [bool](Get-ChildItem $include -Filter Windows.h -Recurse -Depth 2 -ErrorAction SilentlyContinue | Select-Object -First 1)
}

Write-Step 'Looking for the Visual Studio C++ build tools'
$vs = Find-VisualStudio
$missing = @()
if ($vs) {
    if (-not (& $vs.Has $vcTools)) { $missing += $vcTools }
    if ($arch -eq 'arm64' -and -not (Find-Clang $vs.Path)) { $missing += $clangComponent }
}
$needSdk = -not (Test-WindowsSdk)

if (-not $vs) {
    $components = @('--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended', '--add', $vcTools)
    if ($arch -eq 'arm64') { $components += @('--add', $clangComponent) }
    Write-Host 'No Visual Studio or Build Tools found.'
    if (-not (Confirm-Step 'Install the Visual Studio Build Tools with the C++ workload (several GB) using winget?')) {
        throw 'The C++ build tools are needed to build better.'
    }
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
        throw 'winget is not available. Install the Visual Studio Build Tools with "Desktop development with C++" from https://visualstudio.microsoft.com/visual-cpp-build-tools/, then run install.ps1 again.'
    }
    $override = (@('--passive', '--wait') + $components) -join ' '
    Invoke-Native winget.exe @('install', '--id', 'Microsoft.VisualStudio.2022.BuildTools', '--exact', '--source', 'winget',
        '--accept-package-agreements', '--accept-source-agreements', '--override', $override)
} elseif ($missing.Count -gt 0 -or $needSdk) {
    # Adding the C++ workload also brings a Windows SDK.
    $workload = if ($vs.Product -eq 'Microsoft.VisualStudio.Product.BuildTools') { 'Microsoft.VisualStudio.Workload.VCTools' } else { 'Microsoft.VisualStudio.Workload.NativeDesktop' }
    $add = @('--add', $workload, '--includeRecommended')
    foreach ($id in $missing) { $add += @('--add', $id) }
    Write-Host "Visual Studio at $($vs.Path) is missing: $((@($missing) + @(if ($needSdk) { 'a Windows SDK' })) -join ', ')."
    if (-not (Confirm-Step 'Add them with the Visual Studio Installer?')) {
        throw 'These components are needed to build better.'
    }
    $setup = Join-Path $installerDir 'setup.exe'
    $arguments = @('modify', '--installPath', "`"$($vs.Path)`"", '--passive', '--norestart') + $add
    $process = Start-Process $setup -ArgumentList $arguments -Verb RunAs -Wait -PassThru
    # 3010: success, restart recommended.
    if ($process.ExitCode -ne 0 -and $process.ExitCode -ne 3010) { throw "The Visual Studio Installer failed with exit code $($process.ExitCode)." }
}
$vs = Find-VisualStudio
if (-not $vs -or -not (& $vs.Has $vcTools)) { throw "The C++ build tools for $arch are still missing. Add `"$vcTools`" in the Visual Studio Installer." }
Write-Host "Visual Studio at $($vs.Path)."

# On ARM64, ring calls Clang directly, which only finds the Windows headers
# through the variables Visual Studio's ARM64 developer environment sets.
if ($arch -eq 'arm64') {
    Write-Step 'Loading the ARM64 compiler environment'
    $clang = Find-Clang $vs.Path
    if (-not $clang) { throw "Clang not found. Add `"$clangComponent`" in the Visual Studio Installer, or install LLVM." }
    Import-Module (Join-Path $vs.Path 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll')
    Enter-VsDevShell -VsInstallPath $vs.Path -SkipAutomaticLocation -DevCmdArguments "-arch=$vsArch -host_arch=$hostArch -no_logo" | Out-Null
    $env:Path = (Split-Path $clang) + ';' + $env:Path
    Write-Host "Clang at $clang."
}

# --- Rust ---------------------------------------------------------------------
Write-Step 'Looking for Rust'
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$cargoBin = Join-Path $cargoHome 'bin'
if (-not (Get-Command rustup.exe -ErrorAction SilentlyContinue) -and (Test-Path (Join-Path $cargoBin 'rustup.exe'))) {
    # Installed, but this terminal was opened before rustup added it to PATH.
    $env:Path = "$cargoBin;$env:Path"
}
if (-not (Get-Command rustup.exe -ErrorAction SilentlyContinue)) {
    if (-not (Confirm-Step 'Rust is not installed. Install it with rustup?')) { throw 'Rust is needed to build better.' }
    $init = Join-Path ([IO.Path]::GetTempPath()) 'rustup-init.exe'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest "https://static.rust-lang.org/rustup/dist/$target/rustup-init.exe" -OutFile $init -UseBasicParsing
    Invoke-Native $init @('-y', '--profile', 'minimal', '--default-toolchain', 'stable', '--default-host', $target)
    Remove-Item $init
    $env:Path = "$cargoBin;$env:Path"
}
# A toolchain for R's architecture, whatever rustup's default host is.
$toolchain = "stable-$target"
Invoke-Native rustup.exe @('toolchain', 'install', $toolchain, '--profile', 'minimal', '--no-self-update')

# --- Build and install --------------------------------------------------------
Write-Step "Building better for $arch (a first build takes several minutes)"
Invoke-Native cargo.exe @("+$toolchain", 'install', '--path', $root, '--locked')

$better = Join-Path $cargoBin 'better.exe'
Write-Step 'Checking the installed better'
Invoke-Native $better @('--version')

if ($r -and $r.Source -eq 'registry') {
    Write-Host "better finds R through R_HOME or PATH, and neither points to $($r.Home)."
    if (Confirm-Step "Set R_HOME for your user to $($r.Home)?") {
        [Environment]::SetEnvironmentVariable('R_HOME', $r.Home, 'User')
        $env:R_HOME = $r.Home
        $r.Source = 'R_HOME'
    } else {
        Write-Warning "Set R_HOME to $($r.Home), or add R's bin folder to PATH, before running better."
    }
}
if ($r -and $r.Source -ne 'registry') {
    $ErrorActionPreference = 'Continue'
    $out = '1 + 1' | & $better | Out-String
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE -ne 0 -or $out -notmatch '\[1\] 2') { throw "better installed, but running R failed:`n$out" }
    Write-Host 'R runs in better: 1 + 1 gave [1] 2.'
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onUserPath = $userPath -and (($userPath -split ';') -contains $cargoBin)
Write-Host ''
Write-Host "Installed $better." -ForegroundColor Green
if (-not $onUserPath) {
    Write-Warning "$cargoBin is not on your PATH. Add it, or run better by its full path."
} else {
    Write-Host 'Open a new terminal and run: better'
}
