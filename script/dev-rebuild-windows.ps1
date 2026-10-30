# Pulls the latest develop, rebuilds the release binary with the MSVC workarounds
# from docs/src/development/windows-notes.md, and swaps it into place on the Desktop.
#
# Traps this script handles (all hit in practice):
# - Git Bash mangles `CXXFLAGS=/permissive` via MSYS path conversion; we set the
#   env var natively in PowerShell.
# - The running Zed locks its exe; Windows allows renaming a running image, so we
#   rename the old binary instead of killing the process (it may host live agent
#   sessions), then copy the fresh build in.
# - Sibling agents keep uncommitted WIP in this checkout; if the tree is dirty or
#   has local commits we abort instead of stashing away someone's work.
#
# Usage: powershell -NoProfile -File script/dev-rebuild-windows.ps1 [-SkipPull] [-SkipSwap] [-DestPath <path>]

# script/ lives one level below the repo root.
param(
    [string]$RepoRoot = (Split-Path $PSScriptRoot -Parent),
    [string]$DestPath = "$env:USERPROFILE\OneDrive\Desktop\zed.exe",
    [switch]$SkipPull,
    [switch]$SkipSwap
)

$ErrorActionPreference = "Stop"

Set-Location $RepoRoot

if (-not $SkipPull) {
    git fetch origin
    if ($LASTEXITCODE -ne 0) { throw "git fetch failed" }

    $incoming = @(git rev-list HEAD..origin/develop)
    if ($LASTEXITCODE -ne 0) { throw "git rev-list failed" }

    $dirty = @(git status --porcelain)
    if ($dirty.Count -gt 0) {
        throw "Working tree is dirty ($($dirty.Count) paths) - aborting instead of touching possibly-shared WIP. Commit or clean manually, or run with -SkipPull."
    }
    if ($incoming.Count -gt 0) {
        Write-Host "Pulling $($incoming.Count) new commit(s) from origin/develop..."
        git merge --ff-only origin/develop
        if ($LASTEXITCODE -ne 0) { throw "Fast-forward failed (local commits diverged?) - aborting." }
    } else {
        Write-Host "Already up to date."
    }
}

$head = git rev-parse --short HEAD
Write-Host "Building zed at $head (release, CXXFLAGS=/permissive)..."
$env:CXXFLAGS = "/permissive"
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }

$built = Join-Path $RepoRoot "target\release\zed.exe"
if (-not (Test-Path $built)) { throw "Build output missing: $built" }

# The linker embeds the HEAD commit in ProductVersion; use it to prove the exe
# on disk really is this build.
$version = (Get-Item $built).VersionInfo.ProductVersion
Write-Host "Built: $version"
if ($version -notmatch [regex]::Escape($head)) {
    throw "Built exe reports '$version' which does not contain HEAD '$head' - refusing to swap a stale binary."
}

if (-not $SkipSwap) {
    $destDir = Split-Path $DestPath -Parent
    $destName = Split-Path $DestPath -Leaf

    # Archive the PDB next to the build so crash dumps of a deployed exe stay
    # symbolizable; the next build would otherwise overwrite target/release/zed.pdb.
    # Keep only the newest archive, matching the exe backup policy.
    $pdb = Join-Path $RepoRoot "target\release\zed.pdb"
    $pdbArchiveDir = Join-Path $RepoRoot "target\release\pdb-archive"
    if (Test-Path $pdb) {
        New-Item -ItemType Directory -Force -Path $pdbArchiveDir | Out-Null
        Copy-Item $pdb (Join-Path $pdbArchiveDir "$head.pdb") -Force
        Get-ChildItem $pdbArchiveDir -Filter "*.pdb" |
            Sort-Object LastWriteTime -Descending |
            Select-Object -Skip 2 |
            ForEach-Object { Remove-Item $_.FullName -Force }
    }

    $running = Get-Process zed -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $DestPath }
    $backup = $null
    if ($running) {
        # Stamp with the running binary's own HEAD (from its ProductVersion) so
        # same-day reruns can't overwrite the only rollback copy.
        $runningVersion = (Get-Item $DestPath).VersionInfo.ProductVersion
        $runningHead = if ($runningVersion -match '\+dev\.([0-9a-f]{10})') { $Matches[1] } else { Get-Date -Format "yyyyMMdd" }
        $backup = Join-Path $destDir "zed.exe.prev-$runningHead"
        Write-Host "Zed is running from $DestPath - renaming it to '$(Split-Path $backup -Leaf)' (safe while running) and copying the new build in. Restart Zed to pick it up."
        Move-Item $DestPath $backup -Force
    }

    try {
        Copy-Item $built $DestPath -Force
    } catch {
        # OneDrive sync locks are a real risk on the Desktop path; a partial copy
        # here would leave a broken exe. Roll back to the backup we just made.
        if ($backup -and (Test-Path $backup)) {
            Move-Item $backup $DestPath -Force
            Write-Host "Copy failed; restored the previous binary."
        }
        throw
    }

    # Prune swap backups, keep the newest one for rollback.
    Get-ChildItem $destDir -Filter "zed.exe.prev-*" |
        Sort-Object LastWriteTime -Descending |
        Select-Object -Skip 1 |
        ForEach-Object {
            Write-Host "Pruning old backup: $($_.Name)"
            Remove-Item $_.FullName -Force
        }

    $newVersion = (Get-Item $DestPath).VersionInfo.ProductVersion
    Write-Host "Deployed: $DestPath ($newVersion)"
}

Write-Host "Done."
