# Install the Unharnessed CLI on Windows. In PowerShell:
#
#   irm https://github.com/automatissa/unharnessed/releases/latest/download/install.ps1 | iex
#
# It downloads the release, checks its SHA-256, puts it in
# %USERPROFILE%\.unharnessed\app, adds that folder to your user PATH, installs
# uv if it is missing, and prepares the Python kernel. Run it again to update.
# No administrator rights needed. Works in Windows PowerShell 5.1 and
# PowerShell 7.
#
# Settings (environment variables):
#   UNHARNESSED_VERSION        a version like 0.9.8 (default: the latest release)
#   UNHARNESSED_INSTALL_DIR    where the app goes (default: ~\.unharnessed\app)
#   UNHARNESSED_NO_MODIFY_PATH=1   do not change the user PATH
#   UNHARNESSED_NO_UV=1        do not install uv
#   UNHARNESSED_DOWNLOAD_URL   download from this base URL instead of GitHub

function Install-Unharnessed {
    $ErrorActionPreference = 'Stop'
    # The progress bar makes Invoke-WebRequest many times slower in 5.1.
    $ProgressPreference = 'SilentlyContinue'
    $repo = 'automatissa/unharnessed'

    $onWindows = ($PSVersionTable.PSEdition -eq 'Desktop') -or $IsWindows
    if (-not $onWindows) {
        throw "this installer is for Windows; on Linux or macOS run: curl -fsSL https://github.com/$repo/releases/latest/download/install.sh | sh"
    }
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    if ($arch -eq 'ARM64') {
        Write-Host 'Windows on ARM: installing the x64 build, which Windows 11 runs through emulation.'
    } elseif ($arch -ne 'AMD64') {
        throw "unsupported processor: $arch (64-bit Windows is needed)"
    }

    # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $userHome = [Environment]::GetFolderPath('UserProfile')
    if ($env:UNHARNESSED_INSTALL_DIR) { $appDir = $env:UNHARNESSED_INSTALL_DIR } else { $appDir = Join-Path $userHome '.unharnessed\app' }
    if ($env:UNHARNESSED_DOWNLOAD_URL) {
        $base = $env:UNHARNESSED_DOWNLOAD_URL.TrimEnd('/')
    } elseif ($env:UNHARNESSED_VERSION) {
        $base = "https://github.com/$repo/releases/download/v$($env:UNHARNESSED_VERSION.TrimStart('v'))"
    } else {
        $base = "https://github.com/$repo/releases/latest/download"
    }
    $archive = 'unharnessed-windows-x64.zip'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('unharnessed-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $parent = Split-Path -Parent $appDir
    $stage = Join-Path $parent ".unharnessed-install-$PID"
    $old = Join-Path $parent ".unharnessed-old-$PID"
    try {
        Write-Host 'Downloading Unharnessed for windows-x64...'
        $zip = Join-Path $tmp $archive
        try {
            Invoke-WebRequest -Uri "$base/$archive" -OutFile $zip -UseBasicParsing
            Invoke-WebRequest -Uri "$base/$archive.sha256" -OutFile "$zip.sha256" -UseBasicParsing
        } catch {
            throw "could not download $base/$archive ($($_.Exception.Message))"
        }
        $expected = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            throw "the download is damaged (SHA-256 $actual, expected $expected); try again"
        }

        # Unpack beside the destination, then swap folders: an update never
        # leaves a half-written app behind.
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
        if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
        Expand-Archive -Path $zip -DestinationPath $stage -Force
        $stagedExe = Join-Path $stage 'unharnessed\unharnessed.exe'
        if (-not (Test-Path $stagedExe)) { throw 'the archive has no unharnessed.exe' }
        & $stagedExe --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'the downloaded program does not run on this machine' }
        if (Test-Path $old) { Remove-Item -Recurse -Force $old }
        if (Test-Path $appDir) {
            try { Move-Item -Path $appDir -Destination $old }
            catch { throw "could not replace $appDir. Close Unharnessed (and VS Code if it runs it), then try again." }
        }
        try {
            Move-Item -Path (Join-Path $stage 'unharnessed') -Destination $appDir
        } catch {
            if (Test-Path $old) { Move-Item -Path $old -Destination $appDir }
            throw "could not move the app into $appDir ($($_.Exception.Message))"
        }
        Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue

        $exe = Join-Path $appDir 'unharnessed.exe'
        $version = (& $exe --version | Out-String).Trim()
        Write-Host "Installed Unharnessed $version in $appDir."

        # The user PATH, edited in the registry so entries like %USERPROFILE%
        # stay as they are.
        $pathAdded = $false
        if ($env:UNHARNESSED_NO_MODIFY_PATH) {
            Write-Host "Add $appDir to your PATH to run unharnessed."
        } else {
            $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
            try {
                $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                $entries = @($current -split ';' | Where-Object { $_ -ne '' })
                $present = $entries | Where-Object {
                    [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $appDir.TrimEnd('\')
                }
                if (-not $present) {
                    $key.SetValue('Path', (($entries + $appDir) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
                    $pathAdded = $true
                }
            } finally {
                $key.Close()
            }
            if ($pathAdded) {
                # Tell Windows the environment changed, so new terminals see it.
                [Environment]::SetEnvironmentVariable('UNHARNESSED_INSTALLER', '1', 'User')
                [Environment]::SetEnvironmentVariable('UNHARNESSED_INSTALLER', $null, 'User')
                Write-Host "Added $appDir to your user PATH."
            }
        }
        if (-not (@($env:Path -split ';') -contains $appDir)) { $env:Path = "$env:Path;$appDir" }

        # uv builds the Python kernel; its installer puts it in ~\.local\bin.
        $uvDirs = @((Join-Path $userHome '.local\bin'), (Join-Path $userHome '.cargo\bin'))
        foreach ($dir in $uvDirs) {
            if ((Test-Path $dir) -and -not (@($env:Path -split ';') -contains $dir)) { $env:Path = "$env:Path;$dir" }
        }
        if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
            if ($env:UNHARNESSED_NO_UV) {
                Write-Host 'uv is missing: install it (https://docs.astral.sh/uv/) before the first run.'
            } else {
                Write-Host 'Installing uv (it builds the Python kernel)...'
                $shell = (Get-Process -Id $PID).Path
                & $shell -NoProfile -ExecutionPolicy Bypass -Command 'irm https://astral.sh/uv/install.ps1 | iex' | Out-Host
                if ($LASTEXITCODE -ne 0) { Write-Warning 'uv did not install; install it from https://docs.astral.sh/uv/.' }
                foreach ($dir in $uvDirs) {
                    if ((Test-Path $dir) -and -not (@($env:Path -split ';') -contains $dir)) { $env:Path = "$env:Path;$dir" }
                }
            }
        }
        if (Get-Command uv -ErrorAction SilentlyContinue) {
            Write-Host 'Preparing the Python kernel (first time only, about a minute)...'
            $out = Join-Path $tmp 'bootstrap.out'
            $err = Join-Path $tmp 'bootstrap.err'
            # Start-Process, not a redirect: in 5.1 a native command's stderr
            # under 'Stop' would abort the install.
            $bootstrap = Start-Process -FilePath $exe -ArgumentList '--unharnessed-bootstrap' -NoNewWindow -Wait -PassThru `
                -RedirectStandardOutput $out -RedirectStandardError $err
            if ($bootstrap.ExitCode -ne 0) {
                Get-Content -Tail 20 $err, $out -ErrorAction SilentlyContinue | Out-Host
                Write-Warning 'the kernel is not ready yet; Unharnessed retries on its first run.'
            }
        }

        Write-Host ''
        Write-Host 'Done. Open a new terminal, then start it in a project folder with:  unharnessed'
        Write-Host 'It asks you to sign in the first time (or run: unharnessed login).'
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
        if (Test-Path $stage) { Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue }
    }
}

try {
    Install-Unharnessed
} catch {
    Write-Host "Error: $($_.Exception.Message)" -ForegroundColor Red
    # Run as a file (-File), fail the process; piped into iex, leave the
    # person's PowerShell window open.
    if ($PSCommandPath) { exit 1 }
}
