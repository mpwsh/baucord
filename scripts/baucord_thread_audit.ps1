<#
.SYNOPSIS
  Per-thread wake/CPU audit for Baucord (or any process).

  Samples per-thread "Context Switches/sec" and "% Processor Time"
  performance counters once per second for a fixed duration, resolves
  thread names via GetThreadDescription, annotates rates that match the
  known Baucord polling signatures, prints a table, and writes a CSV.

.USAGE
  Start the scenario first (share running, etc.), then:

    .\baucord_thread_audit.ps1 -Label silent_unmuted
    .\baucord_thread_audit.ps1 -Label silent_muted
    .\baucord_thread_audit.ps1 -Label active

  If two baucord processes are running (sharer + viewer on one machine),
  the script lists them - rerun with -ProcessId <pid of the SHARER>.

.NOTES
  - No admin required. PowerShell 5.1 or 7+.
  - Assumes English counter names (localized Windows would need PDH
    index translation - tell Claude if your test box isn't English).
  - Expected signatures (pre-fix build):
      ~500/s  -> encode worker, 2 ms idle poll (ring empty)
      ~100/s  -> encode worker, 10 ms mute-drain poll
      ~10/s   -> sysaudio-procloop 100 ms stop-check (intended floor)
#>

param(
    [string]$ProcessName = "baucord",
    [int]$ProcessId = 0,
    [int]$DurationSeconds = 60,
    [string]$Label = "scenario"
)

$ErrorActionPreference = "Stop"

# -- Thread-name resolution via GetThreadDescription ---------------------
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class ThreadName {
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern IntPtr OpenThread(uint access, bool inherit, uint tid);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern int GetThreadDescription(IntPtr h, out IntPtr desc);
    [DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr h);
    const uint THREAD_QUERY_LIMITED_INFORMATION = 0x0800;
    public static string Get(uint tid) {
        IntPtr h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, tid);
        if (h == IntPtr.Zero) return "";
        try {
            IntPtr p;
            if (GetThreadDescription(h, out p) < 0 || p == IntPtr.Zero) return "";
            string s = Marshal.PtrToStringUni(p);
            LocalFree(p);
            return s == null ? "" : s;
        } finally { CloseHandle(h); }
    }
}
"@

# -- Pick the target process ----------------------------------------------
$procs = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)
if ($procs.Count -eq 0) {
    Write-Host "No '$ProcessName' process found. Start the app (and the share) first." -ForegroundColor Red
    exit 1
}
if ($ProcessId -ne 0) {
    $target = $procs | Where-Object Id -eq $ProcessId
    if (-not $target) { Write-Host "PID $ProcessId is not a '$ProcessName' process." -ForegroundColor Red; exit 1 }
} elseif ($procs.Count -gt 1) {
    Write-Host "Multiple '$ProcessName' processes found - rerun with -ProcessId <pid of the SHARER>:" -ForegroundColor Yellow
    $procs | Select-Object Id, MainWindowTitle, CPU, StartTime | Format-Table -AutoSize
    exit 1
} else {
    $target = $procs[0]
}
$targetPid = $target.Id
Write-Host "Target: $ProcessName (PID $targetPid) | label '$Label' | sampling $DurationSeconds s..." -ForegroundColor Cyan
Write-Host "Keep the scenario steady until sampling finishes." -ForegroundColor Cyan

# -- Kernel/user CPU-time snapshot (before) -------------------------------
function Get-ThreadTimes([int]$ProcId) {
    $map = @{}
    foreach ($t in (Get-Process -Id $ProcId).Threads) {
        $map[[uint32]$t.Id] = @{
            Kernel = $t.PrivilegedProcessorTime.TotalMilliseconds
            User   = $t.UserProcessorTime.TotalMilliseconds
        }
    }
    return $map
}
$timesBefore = Get-ThreadTimes $targetPid

# -- Counter sampling -----------------------------------------------------
$paths = @(
    "\Thread($ProcessName*)\ID Thread",
    "\Thread($ProcessName*)\ID Process",
    "\Thread($ProcessName*)\Context Switches/sec",
    "\Thread($ProcessName*)\% Processor Time"
)
$samples = @(Get-Counter -Counter $paths -SampleInterval 1 -MaxSamples $DurationSeconds `
             -ErrorAction SilentlyContinue)
if ($samples.Count -eq 0) {
    Write-Host "Counter sampling returned nothing (localized Windows?)." -ForegroundColor Red
    exit 1
}

# -- Aggregate per TID, joining instance->TID per sample -----------------
$stats = @{}   # tid -> @{ Csw=list; Cpu=list }
foreach ($s in $samples) {
    $tidMap = @{}; $pidMap = @{}
    foreach ($c in $s.CounterSamples) {
        if     ($c.Path -like "*\id thread")  { $tidMap[$c.InstanceName] = [uint32]$c.CookedValue }
        elseif ($c.Path -like "*\id process") { $pidMap[$c.InstanceName] = [int]$c.CookedValue }
    }
    foreach ($c in $s.CounterSamples) {
        $inst = $c.InstanceName
        if (-not $tidMap.ContainsKey($inst)) { continue }
        if ($pidMap.ContainsKey($inst) -and $pidMap[$inst] -ne $targetPid) { continue }
        $tid = $tidMap[$inst]
        if (-not $stats.ContainsKey($tid)) {
            $stats[$tid] = @{
                Csw = [System.Collections.Generic.List[double]]::new()
                Cpu = [System.Collections.Generic.List[double]]::new()
            }
        }
        if     ($c.Path -like "*\context switches/sec") { $stats[$tid].Csw.Add($c.CookedValue) }
        elseif ($c.Path -like "*\% processor time")     { $stats[$tid].Cpu.Add($c.CookedValue) }
    }
}

# -- Kernel/user CPU-time snapshot (after) --------------------------------
$timesAfter = Get-ThreadTimes $targetPid

# -- Signature annotation (pre-fix build expectations) --------------------
function Get-Note([double]$CswAvg) {
    if ($CswAvg -ge 350 -and $CswAvg -le 650) { return "matches 2ms idle poll (encode worker?)" }
    if ($CswAvg -ge 70  -and $CswAvg -le 140) { return "matches 10ms mute poll (encode worker?)" }
    if ($CswAvg -ge 6   -and $CswAvg -le 15)  { return "matches 100ms wait (procloop floor)" }
    if ($CswAvg -ge 1000)                     { return "hot - likely real work (video/net?)" }
    return ""
}

# -- Build report rows ----------------------------------------------------
$rows = foreach ($tid in $stats.Keys) {
    $csw = $stats[$tid].Csw; $cpu = $stats[$tid].Cpu
    if ($csw.Count -eq 0) { continue }
    $cswAvg = ($csw | Measure-Object -Average).Average
    $kernelMs = 0.0; $userMs = 0.0
    if ($timesBefore.ContainsKey($tid) -and $timesAfter.ContainsKey($tid)) {
        $kernelMs = $timesAfter[$tid].Kernel - $timesBefore[$tid].Kernel
        $userMs   = $timesAfter[$tid].User   - $timesBefore[$tid].User
    }
    [pscustomobject]@{
        TID        = $tid
        Name       = [ThreadName]::Get($tid)
        'CSw/s avg'= [math]::Round($cswAvg, 1)
        'CSw/s max'= [math]::Round(($csw | Measure-Object -Maximum).Maximum, 1)
        'CPU% avg' = [math]::Round(($cpu | Measure-Object -Average).Average, 2)
        'Kern ms'  = [math]::Round($kernelMs, 0)
        'User ms'  = [math]::Round($userMs, 0)
        Samples    = $csw.Count
        Note       = Get-Note $cswAvg
    }
}
$rows = $rows | Sort-Object 'CSw/s avg' -Descending

# -- Output ---------------------------------------------------------------
$stamp = Get-Date -Format "yyyyMMdd_HHmmss"
$csv = Join-Path (Get-Location) "baucord_audit_${Label}_${stamp}.csv"
$rows | Export-Csv -Path $csv -NoTypeInformation

Write-Host ""
Write-Host "=== $Label | PID $targetPid | $($rows.Count) threads | top 20 by context switches/s ===" -ForegroundColor Green
$rows | Select-Object -First 20 | Format-Table -AutoSize

$totalCsw = [math]::Round((($rows | Measure-Object 'CSw/s avg' -Sum).Sum), 0)
$totalCpu = [math]::Round((($rows | Measure-Object 'CPU% avg' -Sum).Sum), 2)
$cores = [Environment]::ProcessorCount
Write-Host "Process totals: ~$totalCsw context switches/s | ~$totalCpu %-of-one-core CPU | $cores logical cores" -ForegroundColor Green
Write-Host "CSV written: $csv" -ForegroundColor Green
Write-Host "Paste the table (or send the CSVs for all scenarios) back to Claude." -ForegroundColor Cyan