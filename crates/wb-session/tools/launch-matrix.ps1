# Launch-path matrix for persistent sessions (design section 14, item 7).
# For each way of starting the broker: create a terminal, report whether the daemon and the host sit
# inside a Job, hard-kill the daemon (and end the launcher chain), and check the host survived.
# Uses throw-away data dirs and pipe names; never touches a real install.
#   powershell -ExecutionPolicy Bypass -File launch-matrix.ps1 [-Bin <dir containing wb-daemon.exe, wb-host.exe, wb.exe>]
param([string]$Bin = (Join-Path $PSScriptRoot '..\target\x86_64-pc-windows-msvc\debug'))
$ErrorActionPreference = 'Stop'
$Bin = (Resolve-Path $Bin).Path
$daemon = Join-Path $Bin 'wb-daemon.exe'
$wb = Join-Path $Bin 'wb.exe'
Add-Type @"
using System; using System.Runtime.InteropServices;
public class JobProbe {
  [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint a, bool i, uint pid);
  [DllImport("kernel32.dll", SetLastError=true)] public static extern bool IsProcessInJob(IntPtr p, IntPtr job, out bool r);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  public static string InJob(uint pid) { IntPtr h = OpenProcess(0x1000, false, pid); if (h == IntPtr.Zero) return "?"; bool r; bool ok = IsProcessInJob(h, IntPtr.Zero, out r); CloseHandle(h); return ok ? (r ? "yes" : "no") : "?"; }
}
"@

function Wb($dir, $pipe, [string[]]$more) { $ErrorActionPreference = 'Continue'; & $wb --data-dir $dir --pipe-name $pipe @more 2>&1 }
function WaitReady($dir, $pipe) {
  for ($i = 0; $i -lt 40; $i++) { $o = Wb $dir $pipe @('ls'); if ($LASTEXITCODE -eq 0) { return $true }; Start-Sleep -Milliseconds 250 }
  return $false
}
function DaemonPid($pipe) {
  (Get-CimInstance Win32_Process -Filter "Name='wb-daemon.exe'" | Where-Object { $_.CommandLine -like "*$pipe*" } | Select-Object -First 1).ProcessId
}

$results = @()
$methods = @('start-process', 'cmd-start', 'cim-create', 'task-scheduler', 'explorer-cmd')
foreach ($m in $methods) {
  $tag = "wbmatrix-$m-$PID"
  $dir = Join-Path $env:TEMP $tag
  $pipe = "\\.\pipe\$tag"
  $daemonArgs = "--data-dir `"$dir`" --pipe-name $pipe"
  $row = [ordered]@{ method = $m; launched = $false; terminal = ''; daemon_in_job = '?'; host_in_job = '?'; host_survives_daemon_kill = ''; note = '' }
  try {
    switch ($m) {
      'start-process' { Start-Process -FilePath $daemon -ArgumentList $daemonArgs -WindowStyle Hidden }
      'cmd-start'     { Start-Process -FilePath cmd.exe -ArgumentList "/c start `"`" /B `"$daemon`" $daemonArgs" -WindowStyle Hidden }
      'cim-create'    { $null = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "`"$daemon`" $daemonArgs" } }
      'task-scheduler' {
        schtasks /create /tn $tag /tr "`"$daemon`" $daemonArgs" /sc once /st 23:59 /f | Out-Null
        schtasks /run /tn $tag | Out-Null
      }
      'explorer-cmd'  {
        $cmd = Join-Path $env:TEMP "$tag.cmd"
        "@echo off`r`nstart `"`" /B `"$daemon`" $daemonArgs" | Set-Content -Encoding ascii $cmd
        Start-Process explorer.exe -ArgumentList "`"$cmd`""
      }
    }
    $row.launched = WaitReady $dir $pipe
    if (-not $row.launched) { $row.note = 'daemon did not answer'; $results += [pscustomobject]$row; continue }
    $dpid = DaemonPid $pipe
    $row.daemon_in_job = [JobProbe]::InJob([uint32]$dpid)
    $new = Wb $dir $pipe @('new', 'm')
    if ($LASTEXITCODE -ne 0) { $row.terminal = 'REFUSED'; $row.note = ($new -join ' ').Trim(); $results += [pscustomobject]$row; continue }
    $row.terminal = 'created'
    $ls = (Wb $dir $pipe @('ls')) -join "`n"
    $hostPid = [uint32]([regex]::Match($ls, 'host=(\d+)').Groups[1].Value)
    $row.host_in_job = [JobProbe]::InJob($hostPid)
    # End the launcher chain: hard-kill the daemon, and stop the scheduled task for that method.
    Stop-Process -Id $dpid -Force
    if ($m -eq 'task-scheduler') { schtasks /end /tn $tag 2>&1 | Out-Null }
    Start-Sleep -Seconds 2
    $alive = [bool](Get-Process -Id $hostPid -ErrorAction SilentlyContinue)
    $row.host_survives_daemon_kill = if ($alive) { 'yes' } else { 'NO' }
    if ($alive) { Stop-Process -Id $hostPid -Force }
  } catch {
    $row.note = $_.Exception.Message
  } finally {
    if ($m -eq 'task-scheduler') { schtasks /delete /tn $tag /f 2>&1 | Out-Null }
    Get-CimInstance Win32_Process -Filter "Name='wb-daemon.exe' OR Name='wb-host.exe'" | Where-Object { $_.CommandLine -like "*$tag*" } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $dir, (Join-Path $env:TEMP "$tag.cmd") -ErrorAction SilentlyContinue
  }
  $results += [pscustomobject]$row
}
$results | Format-Table -AutoSize
