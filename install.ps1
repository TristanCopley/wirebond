# Installs wirebond on Windows, from an administrator PowerShell:
#
#   irm https://raw.githubusercontent.com/TristanCopley/wirebond/main/install.ps1 | iex
#
# It downloads this machine's executable from the latest GitHub release, or from the release that
# $env:WIREBOND_TAG names, checks it against the release's SHA256SUMS, and runs `wirebond install`,
# which puts wirebond in place as a Windows service and its folder on the system PATH. Nothing
# changes on the network until you give the device a role. Running it again updates or mends the
# install. $env:WIREBOND_URL, a folder holding a release's assets and SHA256SUMS, replaces
# GitHub's (a mirror, or a test).
#
# It runs in a script block, so iex leaves no variables behind, and it throws rather than exits,
# which would close the window.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # Windows PowerShell's progress bar slows downloads tenfold
    # Windows PowerShell 5.1 may offer only TLS 1.0, which GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'wirebond installs a Windows service: run this in an administrator PowerShell (right-click PowerShell, then Run as administrator).'
    }
    # A 32-bit PowerShell on 64-bit Windows names the machine's architecture in PROCESSOR_ARCHITEW6432.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -notin 'AMD64', 'ARM64') { throw "There is no wirebond build for $arch Windows." }
    $asset = "wirebond-Windows-$arch.exe"
    $tag = if ($env:WIREBOND_TAG) { $env:WIREBOND_TAG } else { 'latest' }
    $url = if ($env:WIREBOND_URL) {
        $env:WIREBOND_URL.TrimEnd('/')
    } elseif ($tag -eq 'latest') {
        'https://github.com/TristanCopley/wirebond/releases/latest/download'
    } else {
        "https://github.com/TristanCopley/wirebond/releases/download/$tag"
    }

    $dir = Join-Path ([IO.Path]::GetTempPath()) "wirebond-$([Guid]::NewGuid())"
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $exe, $sums = (Join-Path $dir 'wirebond.exe'), (Join-Path $dir 'SHA256SUMS')
        Write-Host "wirebond: downloading $asset ($tag)"
        Invoke-WebRequest -UseBasicParsing -Uri "$url/$asset" -OutFile $exe
        Invoke-WebRequest -UseBasicParsing -Uri "$url/SHA256SUMS" -OutFile $sums
        $want = foreach ($line in Get-Content -LiteralPath $sums) {
            $f = -split $line
            if ($f.Count -ge 2 -and $f[1].TrimStart('*') -eq $asset) { $f[0]; break }
        }
        if (-not $want) { throw "The release's SHA256SUMS does not list $asset." }
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $exe).Hash
        if ($got -ne $want) { throw "$asset does not match SHA256SUMS (got $got, want $want); nothing was installed." }
        # The folder wirebond installs into goes on this window's PATH as well as the system's, so
        # that `wirebond` works here at once.
        $folder = Join-Path $env:ProgramFiles 'wirebond'
        if (($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $folder) {
            $env:Path = $env:Path.TrimEnd(';') + ";$folder"
        }
        & $exe install
        if ($LASTEXITCODE -ne 0) { throw "wirebond install failed (exit code $LASTEXITCODE)." }
    } finally {
        Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    }

    Write-Host @'

Next, in an administrator PowerShell on the device with the good connection, the anchor:
    wirebond anchor on
    wirebond anchor allow <client>
and on the device on the poor connection, the client:
    wirebond client <anchor>
<client> and <anchor> are the devices' names on your mesh (tailscale status lists them).
wirebond status shows the bond; wirebond help lists the rest.
'@
    if (-not (Get-Command tailscale -ErrorAction SilentlyContinue) -and -not (Test-Path (Join-Path $env:ProgramFiles 'Tailscale'))) {
        Write-Warning 'Tailscale is not installed here: wirebond needs a mesh, such as Tailscale, joining the two devices.'
    }
}
