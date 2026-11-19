$ErrorActionPreference = "Stop"
# Regenerate the typed frontend client from the desk-server OpenAPI spec.
#
# The spec is dumped offline via the `dump-openapi` subcommand — it is built
# purely from the route registration (`configure_api_surface`), so no running
# server / DB / Redis / HTTP is required. Works locally and in CI.
#
# The committed client is Linux output: CI runs update_openapi.sh on Ubuntu and
# fails when the result differs. Kubb writes different output on Windows (it
# leaves out the per-tag barrel files and orders imports differently), so on
# Windows the generator runs inside WSL when its default distribution has
# Node.js (on PATH or through nvm), and otherwise this script warns before
# generating natively.
#
# Fix the working directory to this script's location so the spec and Kubb
# config resolve correctly regardless of where the script is invoked from.
Set-Location $PSScriptRoot

$specPath = Join-Path ([System.IO.Path]::GetTempPath()) (
    "lcxl-desk-openapi-{0}.json" -f [System.Guid]::NewGuid()
)
$previousInputPath = $env:KUBB_OPENAPI_PATH
$onWindows = $IsWindows -or $env:OS -eq "Windows_NT"
# Commands run through wsl.exe do not read ~/.bashrc, so load nvm explicitly.
$wslNode = 'if [ -s "$HOME/.nvm/nvm.sh" ]; then . "$HOME/.nvm/nvm.sh" >/dev/null 2>&1; fi;'

# A WSL path for a Windows path, or $null when WSL cannot provide one.
function ConvertTo-WslPath([string]$path) {
    $converted = & wsl.exe -e wslpath -u $path 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $converted) { return $null }
    return ($converted | Select-Object -First 1).Trim()
}

function Test-WslNode {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { return $false }
    $node = & wsl.exe -e bash -c "$wslNode command -v node" 2>$null
    return $LASTEXITCODE -eq 0 -and [bool]$node
}

try {
    # $ErrorActionPreference = "Stop" does not catch native (exe) non-zero
    # exits, so check $LASTEXITCODE after each command.
    Write-Host "Dumping OpenAPI spec (offline)..."
    cargo run -q -p lcxl-remote-desk-server -- dump-openapi --out $specPath
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    # `--no-install` keeps npx from silently fetching a kubb build that is not
    # the one pinned in the lockfile: the committed client must stay
    # reproducible, and a different generator version rewrites it wholesale.
    $wslDirectory = $null
    $wslSpec = $null
    if ($onWindows -and (Test-WslNode)) {
        $wslDirectory = ConvertTo-WslPath $PSScriptRoot
        $wslSpec = ConvertTo-WslPath $specPath
    }
    if ($wslDirectory -and $wslSpec) {
        Write-Host "Generating Kubb clients in WSL (Linux output, as in CI)..."
        & wsl.exe -e bash -c "$wslNode cd '$wslDirectory' && KUBB_OPENAPI_PATH='$wslSpec' npx --no-install kubb generate"
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    else {
        if ($onWindows) {
            Write-Warning (
                "Generating on Windows: Kubb's Windows output differs from the Linux " +
                "output CI checks (missing per-tag index.ts barrels, different import " +
                "order). Install Node.js (for example with nvm) in the default WSL " +
                "distribution and rerun, or " +
                "commit only the changes your API change needs."
            )
        }
        $env:KUBB_OPENAPI_PATH = $specPath
        Write-Host "Generating Kubb clients..."
        npx --no-install kubb generate
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
}
finally {
    if ($null -eq $previousInputPath) {
        Remove-Item Env:KUBB_OPENAPI_PATH -ErrorAction SilentlyContinue
    }
    else {
        $env:KUBB_OPENAPI_PATH = $previousInputPath
    }
    Remove-Item -LiteralPath $specPath -Force -ErrorAction SilentlyContinue
}
