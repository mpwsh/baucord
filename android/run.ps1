<#
.SYNOPSIS
  `cargo run` for the Android app: build the APK, make sure a device is
  there (starting the emulator if needed), install, launch, tail the log.

.DESCRIPTION
  Default: debug build for the x86_64 emulator, first AVD if none is
  running. Ctrl+C ends the log tail; the app keeps running. The build's
  versionCode is the one already installed on the target (an APK from
  CI or the website carries a higher one than a local build's 1).

.EXAMPLE
  .\run.ps1                       # emulator, debug
  .\run.ps1 -Avd "Pixel 10"       # a specific AVD
  .\run.ps1 -Device               # the USB phone instead (arm64, debug)
  .\run.ps1 -Device -Release      # the signed release on the phone
  .\run.ps1 -Hello                # the M0 toolchain smoke test
  .\run.ps1 -NoBuild              # reinstall + relaunch what was last built
  .\run.ps1 -NoLog                # leave without tailing logcat
  .\run.ps1 -ColdBoot             # start the AVD without its quick-boot snapshot

.NOTES
  ANDROID_HOME must point at the SDK (android/README.md section 1). If
  PowerShell refuses to run scripts:
    powershell -ExecutionPolicy Bypass -File .\run.ps1
  or use run.cmd next to this file, which does exactly that.
#>
[CmdletBinding()]
param(
    # Name from `emulator -list-avds`; default: the running one, else the first.
    [string]$Avd = "",
    # Use the USB phone (first non-emulator serial) instead of an AVD.
    [switch]$Device,
    # assembleRelease instead of assembleDebug (needs the signing
    # properties in ~/.gradle/gradle.properties to be installable).
    [switch]$Release,
    # The M0 smoke test instead of the client.
    [switch]$Hello,
    # Override the ABI (defaults: x86_64 for the emulator, arm64-v8a for a phone).
    [string]$Abi = "",
    [switch]$NoBuild,
    [switch]$NoLog,
    # Start the emulator with a full boot instead of its quick-boot
    # snapshot (-no-snapshot-load). Use it when the AVD hangs at
    # "device offline" after "Loading snapshot 'default_boot'": a
    # snapshot saved by an emulator that was killed mid-save restores a
    # guest that never comes back.
    [switch]$ColdBoot
)

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot

function Fail($msg) { Write-Host "run.ps1: $msg" -ForegroundColor Red; exit 1 }

# -- Tools --------------------------------------------------------------
$sdk = $env:ANDROID_HOME
if (-not $sdk) { $sdk = $env:ANDROID_SDK_ROOT }
if (-not $sdk -or -not (Test-Path $sdk)) { Fail "ANDROID_HOME is not set or does not exist (README section 1)" }
$adb = Join-Path $sdk "platform-tools\adb.exe"
$emulator = Join-Path $sdk "emulator\emulator.exe"
if (-not (Test-Path $adb)) { Fail "adb not found at $adb - install Platform-Tools in the SDK Manager" }

if (-not $Abi) { $Abi = if ($Device) { "arm64-v8a" } else { "x86_64" } }
$variant = if ($Release) { "release" } else { "debug" }
$features = if ($Hello) { "hello" } else { "" }

# -- Device -------------------------------------------------------------
function Serials {
    # "emulator-5554\tdevice" lines -> objects; offline/unauthorized excluded.
    & $adb devices | Select-Object -Skip 1 | ForEach-Object {
        $parts = $_ -split "\s+"
        if ($parts.Length -ge 2 -and $parts[1] -eq "device") { $parts[0] }
    }
}

& $adb start-server | Out-Null
$serial = $null
if ($Device) {
    $serial = Serials | Where-Object { $_ -notlike "emulator-*" } | Select-Object -First 1
    if (-not $serial) { Fail "no phone attached (USB debugging on, 'allow' tapped on the phone?)" }
} else {
    $serial = Serials | Where-Object { $_ -like "emulator-*" } | Select-Object -First 1
    if (-not $serial) {
        if (-not (Test-Path $emulator)) { Fail "emulator not found at $emulator - install Emulator in the SDK Manager" }
        $avds = @(& $emulator -list-avds | Where-Object { $_ -and $_ -notmatch "^INFO" })
        if ($avds.Count -eq 0) { Fail "no AVDs - create one in Android Studio's Device Manager (Google APIs, x86_64, API 36)" }
        if (-not $Avd) { $Avd = $avds[0] }
        if ($avds -notcontains $Avd) { Fail "no AVD named '$Avd'; have: $($avds -join ', ')" }
        Write-Host ">> starting emulator '$Avd' (first boot can take a minute)" -ForegroundColor Cyan
        # -allow-host-audio: the AVD's microphone is the PC's (the toolbar's
        # "Virtual microphone uses host audio input", preset). Windows must
        # let desktop apps use the microphone (Privacy settings).
        $emuArgs = @("-avd", "`"$Avd`"", "-netdelay", "none", "-netspeed", "full", "-allow-host-audio")
        if ($ColdBoot) { $emuArgs += "-no-snapshot-load" }
        Start-Process -FilePath $emulator -ArgumentList $emuArgs | Out-Null
        # It boots while the build runs; waited for after it.
    }
}

# -- Version code -------------------------------------------------------
# Never below what the target already has: an APK from CI or the website
# carries its workflow run number, and Android refuses a lower code
# (INSTALL_FAILED_VERSION_DOWNGRADE). The same code installs over it,
# and the next CI build (a higher run number) still installs over this
# one. An emulator started just now has nothing to read: 1.
function InstalledVersionCode($s) {
    $codes = @(& $adb -s $s shell "dumpsys package dev.baucord 2>/dev/null" |
        ForEach-Object { if ("$_" -match 'versionCode=(\d+)') { [int]$Matches[1] } })
    if ($codes.Count -eq 0) { return 0 }
    return ($codes | Measure-Object -Maximum).Maximum
}
$versionCode = 1
if ($serial) {
    $installed = InstalledVersionCode $serial
    if ($installed -gt $versionCode) { $versionCode = $installed }
}

# -- Build --------------------------------------------------------------
if (-not $NoBuild) {
    $task = if ($Release) { "assembleRelease" } else { "assembleDebug" }
    Write-Host ">> gradlew $task  (abi $Abi$(if ($features) { ", features $features" }))" -ForegroundColor Cyan
    Push-Location $here
    try {
        $gradleArgs = @($task, "-PcargoTargets=$Abi", "-PcargoFeatures=$features")
        if ($versionCode -gt 1) {
            Write-Host ">> versionCode $versionCode (what $serial has installed)" -ForegroundColor Cyan
            $gradleArgs += "-PbaucordVersionCode=$versionCode"
        }
        & (Join-Path $here "gradlew.bat") @gradleArgs
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    } finally { Pop-Location }
}

$apkDir = Join-Path $here "app\build\outputs\apk\$variant"
$apk = Join-Path $apkDir "app-$variant.apk"
if (-not (Test-Path $apk)) {
    $unsigned = Join-Path $apkDir "app-$variant-unsigned.apk"
    if (Test-Path $unsigned) {
        Fail "only an unsigned release APK exists ($unsigned); adb refuses those - set the BAUCORD_* signing properties (README section 4) or build debug"
    }
    Fail "no APK at $apk - build first (drop -NoBuild)"
}

# -- Wait for the emulator ----------------------------------------------
if (-not $Device) {
    if (-not $serial) {
        $deadline = (Get-Date).AddMinutes(3)
        while (-not $serial -and (Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 2
            $serial = Serials | Where-Object { $_ -like "emulator-*" } | Select-Object -First 1
        }
        if (-not $serial) { Fail "the emulator did not show up in adb within 3 minutes - if it sits at 'device offline', close it and try .\run.ps1 -ColdBoot" }
    }
    # Wait for Android itself, not just adb.
    $deadline = (Get-Date).AddMinutes(3)
    do {
        $booted = (& $adb -s $serial shell getprop sys.boot_completed 2>$null) -join ""
        if ($booted.Trim() -ne "1") { Start-Sleep -Seconds 2 }
    } while ($booted.Trim() -ne "1" -and (Get-Date) -lt $deadline)
    if ($booted.Trim() -ne "1") { Fail "emulator did not finish booting - close it and try .\run.ps1 -ColdBoot" }
}
Write-Host ">> device $serial" -ForegroundColor Cyan

# -- Install + launch ---------------------------------------------------
Write-Host ">> adb install -r $(Split-Path $apk -Leaf)" -ForegroundColor Cyan
# adb reports the reason on stderr; read it (Continue: under Stop, a
# native command's stderr through 2>&1 throws in Windows PowerShell).
$ErrorActionPreference = "Continue"
$installOut = @(& $adb -s $serial install -r $apk 2>&1 | ForEach-Object { "$_" })
$installExit = $LASTEXITCODE
$ErrorActionPreference = "Stop"
$installOut | ForEach-Object { Write-Host $_ }
if ($installExit -ne 0) {
    $why = $installOut -join "`n"
    if ($why -match "INSTALL_FAILED_VERSION_DOWNGRADE") {
        Fail "$serial has a newer build of dev.baucord. Rebuild (drop -NoBuild): run.ps1 builds with the installed versionCode."
    }
    if ($why -match "INSTALL_FAILED_UPDATE_INCOMPATIBLE|signatures do not match") {
        Fail "the installed app is signed with another key (a debug build over a release, or the reverse). 'adb uninstall dev.baucord' first - it deletes the app's data, its identity included (Settings > Identity > Export to keep it)."
    }
    Fail "install failed (see above)"
}

& $adb -s $serial shell am force-stop dev.baucord | Out-Null
& $adb -s $serial logcat -c
# What tapping the icon does: ask the package manager for the launcher
# Activity, then start that one by name, so this never hard-codes the
# class. (An implicit `am start ... dev.baucord` does not work: starting
# an Activity only matches filters that also list category.DEFAULT,
# which a launcher entry has no reason to.) Both commands report
# failures as text with exit code 0, so stderr is merged on the device
# (a local 2>&1 would throw under ErrorActionPreference Stop) and the
# words are checked. `--brief` prints the component on its last line.
$resolved = @(& $adb -s $serial shell "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER dev.baucord 2>&1" |
    ForEach-Object { "$_".Trim() } | Where-Object { $_ })
$component = $resolved | Select-Object -Last 1
if ($component -notmatch '^dev\.baucord/') {
    Fail "no launcher Activity for dev.baucord on $serial (is it installed? adb shell pm list packages dev.baucord):`n$($resolved -join "`n")"
}
$started = (& $adb -s $serial shell "am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n $component 2>&1") -join "`n"
if ($LASTEXITCODE -ne 0 -or $started -match "Error|Exception") { Fail "could not start ${component}:`n$started" }

if ($NoLog) { exit 0 }
# Ours plus the native crash dumper (DEBUG) - Ctrl+C to stop; the app stays up.
Write-Host ">> logcat (Ctrl+C to stop)" -ForegroundColor Cyan
& $adb -s $serial logcat -v time -s baucord DEBUG
