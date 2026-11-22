# Renders x76tracker's standard screenshots without opening a window, using the app's own
# --screenshot mode (needs a build made with --features docs), from a temporary copy of your data (the running app's history is never touched).
#
#   .\docs\screenshots.ps1                       dark theme, into docs\images
#   .\docs\screenshots.ps1 -Theme light -Out C:\shots
#
# What shows up is your real data: your x76 profile, tracked players and, on the Discord widget
# tab, your Application ID and Discord user ID. Look the images over before publishing them.
param(
    [string]$Exe = (Join-Path $PSScriptRoot "..\target\release\x76tracker.exe"),
    [string]$Out = (Join-Path $PSScriptRoot "images"),
    [ValidateSet("dark", "light")][string]$Theme = "dark",
    [string]$Size = "1440x1000"
)

# --screenshot only exists in builds with the docs feature (release builds leave it out)
if (-not (Test-Path $Exe)) { throw "No x76tracker.exe at $Exe. Build it first (cargo build --release --features docs) or pass -Exe." }
$data = Join-Path $env:TEMP "x76tracker-shots-data"
if (Test-Path $data) { Remove-Item $data -Recurse -Force }
robocopy (Join-Path $env:LOCALAPPDATA "x76tracker") $data /E /XF app.lock /NFL /NDL /NJH /NJS /NP | Out-Null
New-Item -ItemType Directory -Force $Out | Out-Null

$shots = [ordered]@{
    "ladders"          = @()
    "result-detail"    = @("--detail")
    "servers"          = @("--tab", "servers")
    "settings-general" = @("--settings", "general")
    "settings-widget"  = @("--settings", "widget")
    "settings-updates" = @("--settings", "updates")
}
foreach ($name in $shots.Keys) {
    $file = Join-Path (Resolve-Path $Out) "$name-$Theme.png"
    $argList = @("--data-dir", "`"$data`"", "--screenshot", "`"$file`"", "--size", $Size, "--$Theme") + $shots[$name]
    Start-Process -FilePath $Exe -ArgumentList $argList -Wait -NoNewWindow
    if (Test-Path $file) { Write-Host "rendered $file" } else { Write-Warning "no image for $name" }
}
Remove-Item $data -Recurse -Force
exit 0 # robocopy's "copied" code would otherwise read as a failure
