# Chena-Shell release build (ASCII only: avoids PowerShell 5 encoding issues)
# Every published artifact is created and verified by this invocation. Existing
# release directories are never removed or reused.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$srcTauri = Join-Path $root 'src-tauri'
$targetDir = Join-Path $srcTauri 'target'
$env:CARGO_TARGET_DIR = $targetDir

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$NativeArgs,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory
    )

    Push-Location $WorkingDirectory
    try {
        & $FilePath @NativeArgs
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) {
            throw "$FilePath failed with exit code ${exitCode}: $NativeArgs"
        }
    }
    finally {
        Pop-Location
    }
}

function Assert-ChenaText {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $text = [string]$Value
    if ([string]::IsNullOrWhiteSpace($text) -or
        $text.IndexOf('Chena', [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw "$Label must contain 'Chena'. Actual value: '$text'"
    }
}

function Assert-FreshFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][datetime]$NotBeforeUtc
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required build artifact was not created: $Path"
    }
    $file = Get-Item -LiteralPath $Path
    if ($file.Length -le 0) {
        throw "Build artifact is empty: $Path"
    }
    if ($file.LastWriteTimeUtc -lt $NotBeforeUtc) {
        throw "Build artifact is stale; refusing to publish an old file: $Path"
    }
    return $file
}

function Assert-WindowsFileMetadata {
    param([Parameter(Mandatory = $true)][string]$Path)

    $vi = (Get-Item -LiteralPath $Path).VersionInfo
    Write-Host "  Metadata: $Path"
    Write-Host ("    ProductName     : {0}" -f $vi.ProductName)
    Write-Host ("    FileDescription : {0}" -f $vi.FileDescription)
    Write-Host ("    CompanyName     : {0}" -f $vi.CompanyName)
    Write-Host ("    FileVersion     : {0}" -f $vi.FileVersion)
    Write-Host ("    LegalCopyright  : {0}" -f $vi.LegalCopyright)

    Assert-ChenaText $vi.ProductName 'ProductName'
    Assert-ChenaText $vi.FileDescription 'FileDescription'
    Assert-ChenaText $vi.CompanyName 'CompanyName'
    Assert-ChenaText $vi.LegalCopyright 'LegalCopyright'
    return $vi
}

function Test-ChenaApplication {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label application executable was not found: $Path"
    }
    $process = Start-Process -FilePath $Path -PassThru
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        while ([DateTime]::UtcNow -lt $deadline) {
            $process.Refresh()
            if ($process.HasExited) {
                throw "$Label application exited during startup with code $($process.ExitCode)."
            }
            if ($process.MainWindowHandle -ne 0 -and
                [string]$process.MainWindowTitle -match 'Chena-Shell') {
                break
            }
            Start-Sleep -Milliseconds 250
        }
        $process.Refresh()
        if ($process.HasExited) {
            throw "$Label application exited during startup with code $($process.ExitCode)."
        }
        if ($process.MainWindowHandle -eq 0) {
            throw "$Label application has no visible main window after startup."
        }
        if ([string]$process.MainWindowTitle -notmatch 'Chena-Shell') {
            throw "$Label main window title is unexpected: '$($process.MainWindowTitle)'"
        }
        Write-Host "  $Label startup smoke passed (PID $($process.Id), window '$($process.MainWindowTitle)')." -ForegroundColor Green
    }
    finally {
        if (-not $process.HasExited) {
            [void]$process.CloseMainWindow()
            if (-not $process.WaitForExit(5000)) {
                $process.Kill()
                $process.WaitForExit()
            }
        }
    }
}

$npm = (Get-Command npm.cmd -ErrorAction Stop).Source
$cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
if ($null -ne $cargoCommand) {
    $cargo = $cargoCommand.Source
}
else {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    throw "Rust cargo executable not found: $cargo"
}
# Tauri starts Cargo as a child process, so resolving $cargo alone is not enough.
$cargoBinDir = Split-Path -Parent $cargo
$env:Path = "$cargoBinDir;$env:Path"

Write-Host '[1/9] Validating release metadata...' -ForegroundColor Cyan
$configPath = Join-Path $srcTauri 'tauri.conf.json'
$packagePath = Join-Path $root 'package.json'
$config = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
$package = Get-Content -LiteralPath $packagePath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($config.productName -cne 'Chena-Shell' -or $config.mainBinaryName -cne 'Chena-Shell') {
    throw 'productName and mainBinaryName must both be Chena-Shell.'
}
Assert-ChenaText $config.bundle.publisher 'bundle.publisher'
Assert-ChenaText $config.bundle.copyright 'bundle.copyright'
if ([string]$config.bundle.shortDescription -notmatch 'Chena-Shell' -or
    [string]$config.bundle.shortDescription -notmatch 'Chena-Sftp') {
    throw 'bundle.shortDescription must name both Chena-Shell and Chena-Sftp.'
}
if (@($config.bundle.targets).Count -ne 0) {
    throw 'This project publishes the portable build only; bundle.targets must be empty.'
}
if ($package.name -cne 'chena-shell' -or $package.version -cne $config.version) {
    throw 'package.json name/version do not match the Chena-Shell release configuration.'
}
# 发版版本采用四段式 V1.0.0.0；前三位必须与内部 semver 一致。
$releaseVersion = [string]$package.releaseVersion
if ($releaseVersion -notmatch '^V\d+\.\d+\.\d+\.\d+$') {
    throw "package.json releaseVersion must look like V1.0.0.0; actual value: '$releaseVersion'"
}
$releaseTriplet = ($releaseVersion.Substring(1) -split '\.')[0..2] -join '.'
if ($releaseTriplet -ne $package.version) {
    throw "releaseVersion '$releaseVersion' must start with internal version '$($package.version)'."
}

Write-Host '[2/9] Running frontend lint gate...' -ForegroundColor Cyan
Invoke-Native $npm @('run', 'lint') $root
Write-Host '[3/9] Running frontend test gate...' -ForegroundColor Cyan
Invoke-Native $npm @('test') $root
Write-Host '[4/9] Running frontend production build gate...' -ForegroundColor Cyan
Invoke-Native $npm @('run', 'build') $root
Write-Host '[5/9] Running Rust test gate...' -ForegroundColor Cyan
Invoke-Native $cargo @('test', '--locked') $srcTauri
Write-Host '[6/9] Running Rust release check gate...' -ForegroundColor Cyan
Invoke-Native $cargo @('check', '--release', '--locked') $srcTauri

Write-Host '[7/9] Building the portable application with Tauri...' -ForegroundColor Cyan
# 尽力关闭运行中的实例；僵尸/无权限进程不应中断发布构建。
Get-Process -Name 'Chena-Shell*' -ErrorAction SilentlyContinue | ForEach-Object {
    try {
        Stop-Process -Id $_.Id -Force -ErrorAction Stop
    } catch {
        Write-Warning "无法结束进程 $($_.Exception.Message)；继续构建。"
    }
}
# Force the application target to relink without deleting any published release.
Invoke-Native $cargo @('clean', '--release', '-p', 'chena-shell') $srcTauri
$bundleStartUtc = [DateTime]::UtcNow
Invoke-Native $npm @('run', 'tauri', '--', 'build', '--no-bundle', '--ci', '--', '--locked') $root

$exeSrc = Join-Path $targetDir 'release\Chena-Shell.exe'
$exe = Assert-FreshFile $exeSrc $bundleStartUtc
$exeVersion = Assert-WindowsFileMetadata $exe.FullName
if ($exeVersion.ProductName -cne 'Chena-Shell') {
    throw "EXE ProductName must be 'Chena-Shell'. Actual value: '$($exeVersion.ProductName)'"
}

Write-Host '[8/9] Creating and verifying portable ZIP and release directory...' -ForegroundColor Cyan
$releaseRoot = Join-Path $root 'release'
if (-not (Test-Path -LiteralPath $releaseRoot -PathType Container)) {
    New-Item -ItemType Directory -Path $releaseRoot | Out-Null
}
$buildId = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmssfff') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$artifactBase = "Chena-Shell_$releaseVersion"
$exeName = "$artifactBase.exe"
$zipName = "${artifactBase}_portable.zip"
$buildStamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$finalDir = Join-Path $releaseRoot "${artifactBase}_$buildStamp"
$stageDir = Join-Path $releaseRoot ".staging-$buildId"
New-Item -ItemType Directory -Path $stageDir -ErrorAction Stop | Out-Null

$exeDst = Join-Path $stageDir $exeName
$readmeDst = Join-Path $stageDir 'PORTABLE-README.txt'
$zipPath = Join-Path $stageDir $zipName
Copy-Item -LiteralPath $exe.FullName -Destination $exeDst

$portableDocSources = [ordered]@{
    'AI-使用说明.md' = Join-Path $root 'docs\AI-使用说明.md'
    'SKILL.md' = Join-Path $root '.trae\skills\chena-shell\SKILL.md'
    '使用手册.md' = Join-Path $root 'docs\使用手册.md'
    '更新记录.md' = Join-Path $root 'docs\更新记录.md'
}
$portableDocNames = @($portableDocSources.Keys)
foreach ($name in $portableDocNames) {
    $source = $portableDocSources[$name]
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Required portable documentation was not found: $source"
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $stageDir $name)
}
@(
    'Chena-Shell / Chena-Sftp 绿色版',
    "版本：$releaseVersion（内部 semver $($package.version)）",
    '',
    '文件说明：',
    "- $exeName：主程序，双击运行，无需安装。",
    '- 使用手册.md：普通用户的安装、终端、SFTP 与 MCP 配置说明。',
    '- AI-使用说明.md：供 AI agent 阅读的 MCP 接入、Token 配置与工具调用说明。',
    '- SKILL.md：供 AI agent 自安装的 Chena-Shell skill。',
    '- 更新记录.md：版本变更、修复与升级注意事项。',
    '- PORTABLE-README.txt：本说明。',
    '',
    '让其他 AI agent 接入：',
    '1. 完整解压本压缩包，不要只复制 EXE。',
    '2. 把 AI-使用说明.md 和 SKILL.md 一起交给 agent。',
    '3. agent 应引导你在「设置 → MCP服务」启用服务、生成访问凭证，并把凭证填入 MCP 客户端。',
    '4. MCP 访问凭证不是 session_attach 返回的 attach_token，不能混用。优先由用户本人把凭证直接填入客户端，不要发到聊天或提交到仓库。',
    '',
    '运行需要 Microsoft Edge WebView2 Runtime。如果系统未安装，请从微软官方页面安装：',
    'https://developer.microsoft.com/microsoft-edge/webview2/'
) | Set-Content -LiteralPath $readmeDst -Encoding UTF8

$portableFiles = @($exeDst, $readmeDst) + @($portableDocNames | ForEach-Object { Join-Path $stageDir $_ })
Compress-Archive -LiteralPath $portableFiles -DestinationPath $zipPath -CompressionLevel Optimal
[void](Assert-FreshFile $zipPath $bundleStartUtc)
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    $entries = @($archive.Entries)
    $expectedEntries = @($exeName, 'PORTABLE-README.txt') + $portableDocNames
    if ($entries.Count -ne $expectedEntries.Count) {
        throw "Portable ZIP must contain exactly $($expectedEntries.Count) files; found $($entries.Count)."
    }
    foreach ($name in $expectedEntries) {
        $entry = $archive.GetEntry($name)
        if ($null -eq $entry -or $entry.Length -le 0) {
            throw "Portable ZIP is missing required file: $name"
        }
    }
    $zipExe = $archive.GetEntry($exeName)
    if ($zipExe.Length -ne $exe.Length) {
        throw "Portable ZIP does not contain the validated $exeName."
    }
}
finally {
    $archive.Dispose()
}

Write-Host '[9/9] Smoke testing the portable ZIP...' -ForegroundColor Cyan
$smokeRoot = Join-Path ([IO.Path]::GetTempPath()) "Chena-Shell-release-smoke-$buildId"
$appDataDir = Join-Path $env:APPDATA 'chena-shell'
$appDataExisted = Test-Path -LiteralPath $appDataDir
$smokePassed = $false
New-Item -ItemType Directory -Path $smokeRoot -ErrorAction Stop | Out-Null
try {
    $portableDir = Join-Path $smokeRoot 'portable'
    Expand-Archive -LiteralPath $zipPath -DestinationPath $portableDir -ErrorAction Stop
    Test-ChenaApplication (Join-Path $portableDir $exeName) 'Portable ZIP'
    $smokePassed = $true
}
finally {
    Get-Process -Name 'Chena-Shell*' -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            Stop-Process -Id $_.Id -Force -ErrorAction Stop
        } catch {
            Write-Warning "无法结束进程 $($_.Exception.Message)；继续清理。"
        }
    }
    if (Test-Path -LiteralPath $smokeRoot) {
        if ($smokePassed) {
            # 冒烟目录位于 %TEMP%，清理失败不应让已通过的发布整体失败。
            try {
                Remove-Item -LiteralPath $smokeRoot -Recurse -Force -ErrorAction Stop
            } catch {
                Write-Warning "冒烟临时目录清理失败，可手动删除：$smokeRoot（$($_.Exception.Message)）"
            }
        }
        else {
            Write-Host "Smoke-test directory preserved for diagnostics: $smokeRoot" -ForegroundColor Yellow
        }
    }
    if (-not $appDataExisted -and (Test-Path -LiteralPath $appDataDir)) {
        try {
            Remove-Item -LiteralPath $appDataDir -Recurse -Force -ErrorAction Stop
        } catch {
            Write-Warning "冒烟产生的应用数据目录清理失败，可手动删除：$appDataDir（$($_.Exception.Message)）"
        }
    }
}

Move-Item -LiteralPath $stageDir -Destination $finalDir -ErrorAction Stop
$finalExe = Join-Path $finalDir $exeName
$finalZip = Join-Path $finalDir $zipName
Write-Host 'Release completed successfully.' -ForegroundColor Green
Write-Host "  Portable EXE: $finalExe"
Write-Host "  Portable ZIP: $finalZip"
