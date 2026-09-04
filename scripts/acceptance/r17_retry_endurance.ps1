[CmdletBinding()]
param(
    [string]$RunwatchExe = "target/release/runwatch.exe",
    [string]$SlurmHost = "gm00",
    [string]$RemoteRoot = "/share/home/shark/runwatch-r17-endurance",
    [string]$AuthorityId = "",
    [int]$TargetSeconds = 7200,
    [int]$SegmentSeconds = 1800,
    [int]$DelaySeconds = 600,
    [int]$CrashAfterSeconds = 30,
    [int]$MinRounds = 10,
    [switch]$Resume,
    [switch]$Status,
    [switch]$PlanOnly
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"

$ScriptSchema = 1
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$DriverPath = (Resolve-Path $PSCommandPath).Path
$OutputRoot = Join-Path $RepoRoot "dist/r17-endurance"

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Invoke-Git([string[]]$GitArgs) {
    $output = & git.exe -C $RepoRoot @GitArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "git $($GitArgs -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join "`n").Trim()
}

function Assert-CleanWorktree {
    $dirty = Invoke-Git @("status", "--porcelain", "--untracked-files=all")
    if (-not [string]::IsNullOrWhiteSpace($dirty)) {
        throw "R17 endurance authority requires a clean worktree; commit or remove changes first.`n$dirty"
    }
}

function Write-AtomicJson([string]$Path, $Value) {
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $tmp = "$Path.tmp.$PID"
    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($tmp, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $tmp -Destination $Path -Force
}

function Read-State([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "authority state does not exist: $Path"
    }
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Quote-Sh([string]$Value) {
    $doubleQuote = [char]34
    $replacement = "'" + $doubleQuote + "'" + $doubleQuote + "'"
    return "'" + $Value.Replace("'", $replacement) + "'"
}

function Invoke-Ssh([string]$Command) {
    $output = & ssh.exe -o ForwardX11=no $SlurmHost $Command 2>&1
    $code = $LASTEXITCODE
    return [pscustomobject]@{ code = $code; output = ($output -join "`n") }
}

function Assert-Ssh([string]$Command, [string]$Label) {
    $result = Invoke-Ssh $Command
    if ($result.code -ne 0) {
        throw "$Label failed with exit $($result.code): $($result.output)"
    }
    return $result.output
}

function Format-SlurmTime([int]$Seconds) {
    $seconds = [Math]::Max(60, $Seconds)
    $hours = [Math]::Floor($seconds / 3600)
    $minutes = [Math]::Floor(($seconds % 3600) / 60)
    $remaining = $seconds % 60
    return "{0:D2}:{1:D2}:{2:D2}" -f $hours, $minutes, $remaining
}

function Get-PidFromHeartbeat([string]$DataDir, [string]$Name) {
    $path = Join-Path $DataDir $Name
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    try {
        return [int](Get-Content -LiteralPath $path -Raw | ConvertFrom-Json).pid
    } catch {
        return $null
    }
}

function Invoke-Rw([string]$Endpoint, [string]$Op, [hashtable]$Fields = @{}) {
    $prefix = "\\.\pipe\"
    if (-not $Endpoint.StartsWith($prefix)) {
        throw "unsupported Windows endpoint $Endpoint"
    }
    $pipeName = $Endpoint.Substring($prefix.Length)
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $client.Connect(5000)
        $writer = New-Object System.IO.StreamWriter($client, (New-Object System.Text.UTF8Encoding($false)), 4096, $true)
        $reader = New-Object System.IO.StreamReader($client, [System.Text.Encoding]::UTF8, $false, 4096, $true)
        $writer.AutoFlush = $true
        $request = [ordered]@{ id = [guid]::NewGuid().ToString('N'); op = $Op }
        foreach ($key in $Fields.Keys) { $request[$key] = $Fields[$key] }
        $writer.WriteLine(($request | ConvertTo-Json -Compress -Depth 24))
        $line = $reader.ReadLine()
        if ([string]::IsNullOrWhiteSpace($line)) {
            throw "empty IPC response for $Op"
        }
        $response = $line | ConvertFrom-Json
        if (-not $response.ok) {
            throw "IPC $Op failed: $($response.error)"
        }
        return $response.result
    } finally {
        $client.Dispose()
    }
}

function Wait-Hello([string]$Endpoint, [int]$Seconds = 40) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    do {
        try {
            $hello = Invoke-Rw $Endpoint "hello"
            if ($hello.version) { return $hello }
        } catch {}
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "runwatchd IPC did not become ready within ${Seconds}s"
}

function Wait-RunStatus(
    [string]$Endpoint,
    [string]$RunId,
    [string[]]$Expected,
    [int]$TimeoutSeconds
) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $run = (Invoke-Rw $Endpoint "get_run" @{ run_id = $RunId }).run
        if ($Expected -contains [string]$run.status) { return $run }
        if (@("succeeded", "failed", "cancelled") -contains [string]$run.status) {
            throw "Run $RunId reached unexpected terminal status $($run.status); expected $($Expected -join '/')"
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "Run $RunId did not reach $($Expected -join '/') within ${TimeoutSeconds}s"
}

function Stop-Resident($Context) {
    $supervisorPid = Get-PidFromHeartbeat $Context.data_dir "supervise.pid"
    if ($supervisorPid) {
        Stop-Process -Id $supervisorPid -Force -ErrorAction SilentlyContinue
    }
    & schtasks.exe /End /TN $Context.task_name 2>$null | Out-Null
    for ($i = 0; $i -lt 100; $i++) {
        $servePid = Get-PidFromHeartbeat $Context.data_dir "serve.pid"
        $superPid = Get-PidFromHeartbeat $Context.data_dir "supervise.pid"
        $alive = $false
        foreach ($candidate in @($servePid, $superPid)) {
            if ($candidate -and (Get-Process -Id $candidate -ErrorAction SilentlyContinue)) {
                $alive = $true
            }
        }
        if (-not $alive) { return }
        Start-Sleep -Milliseconds 100
    }
    throw "resident processes did not stop within 10 seconds"
}

function Write-ResidentTask($Context) {
    New-Item -ItemType Directory -Force -Path $Context.segment_dir | Out-Null
    $wrapper = $Context.wrapper_path
    @"
@echo off
set "RUNWATCH_DATA_DIR=$($Context.data_dir)"
set "RUNWATCH_ENDPOINT=$($Context.endpoint)"
cd /d "$($Context.runtime_dir)"
"$($Context.runtime_exe)" supervise --interval 1 1>>"$($Context.supervisor_stdout)" 2>>"$($Context.supervisor_stderr)"
"@ | Set-Content -LiteralPath $wrapper -Encoding ASCII

    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $user = [Security.SecurityElement]::Escape($identity.Name)
    $sid = $identity.User.Value
    $security = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$sid)"
    $start = (Get-Date).AddDays(1).ToString("yyyy-MM-ddTHH:mm:ss")
    $command = [Security.SecurityElement]::Escape("C:\Windows\System32\cmd.exe")
    $arguments = [Security.SecurityElement]::Escape("/d /c `"$wrapper`"")
    $working = [Security.SecurityElement]::Escape($Context.runtime_dir)
    $xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>R17 durable Retry endurance authority.</Description><SecurityDescriptor>$security</SecurityDescriptor></RegistrationInfo>
  <Triggers><TimeTrigger><StartBoundary>$start</StartBoundary><Enabled>true</Enabled></TimeTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>$user</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>true</Hidden><RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>7</Priority></Settings>
  <Actions Context="Author"><Exec><Command>$command</Command><Arguments>$arguments</Arguments><WorkingDirectory>$working</WorkingDirectory></Exec></Actions>
</Task>
"@
    [IO.File]::WriteAllText($Context.xml_path, $xml, [Text.Encoding]::Unicode)
    & schtasks.exe /Delete /TN $Context.task_name /F 2>$null | Out-Null
    & schtasks.exe /Create /TN $Context.task_name /XML $Context.xml_path /F | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "failed to create Task Scheduler task $($Context.task_name)" }
}

function Start-Resident($Context) {
    & schtasks.exe /Run /TN $Context.task_name | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "failed to start Task Scheduler task $($Context.task_name)" }
    [void](Wait-Hello $Context.endpoint 40)
    $supervisorPid = Get-PidFromHeartbeat $Context.data_dir "supervise.pid"
    $servePid = Get-PidFromHeartbeat $Context.data_dir "serve.pid"
    if (-not $supervisorPid -or -not $servePid) { throw "resident pid files are missing" }
    $supervisor = Get-Process -Id $supervisorPid -ErrorAction Stop
    $serve = Get-Process -Id $servePid -ErrorAction Stop
    if ($supervisor.SessionId -le 0 -or $serve.SessionId -ne $supervisor.SessionId) {
        throw "resident is not in one interactive desktop session: supervisor=$($supervisor.SessionId) serve=$($serve.SessionId)"
    }
    return [pscustomobject]@{
        supervisor_pid = $supervisorPid
        serve_pid = $servePid
        session_id = $serve.SessionId
    }
}

function Remove-ResidentTask($Context) {
    try { Stop-Resident $Context } catch {}
    & schtasks.exe /Delete /TN $Context.task_name /F 2>$null | Out-Null
}

function New-ResidentContext($State, [string]$SegmentId) {
    $segmentDir = Join-Path $State.authority_root ("segments/" + $SegmentId)
    return [pscustomobject]@{
        authority_root = $State.authority_root
        segment_dir = $segmentDir
        data_dir = $State.data_dir
        endpoint = $State.endpoint
        task_name = $State.task_name
        runtime_dir = (Split-Path -Parent $State.runtime_path)
        runtime_exe = $State.runtime_path
        wrapper_path = (Join-Path $segmentDir "resident.cmd")
        xml_path = (Join-Path $segmentDir "task.xml")
        supervisor_stdout = (Join-Path $segmentDir "supervisor.stdout.log")
        supervisor_stderr = (Join-Path $segmentDir "supervisor.stderr.log")
    }
}

function Assert-AuthorityIdentity($State) {
    Assert-CleanWorktree
    $head = Invoke-Git @("rev-parse", "HEAD")
    if ($head -ne $State.git_commit) { throw "git commit changed: authority=$($State.git_commit) current=$head" }
    $driverHash = Get-Sha256 $DriverPath
    if ($driverHash -ne $State.driver_sha256) { throw "endurance driver hash changed; authority is frozen" }
    if (-not (Test-Path -LiteralPath $State.runtime_path)) { throw "frozen runtime is missing: $($State.runtime_path)" }
    $runtimeHash = Get-Sha256 $State.runtime_path
    if ($runtimeHash -ne $State.runtime_sha256) { throw "frozen runtime hash changed; authority is invalid" }
    if ($State.slurm_host -ne $SlurmHost -or $State.remote_root -ne $RemoteRoot) {
        throw "host/workspace contract changed; authority is frozen"
    }
    if ([int]$State.target_seconds -ne $TargetSeconds -or [int]$State.delay_seconds -ne $DelaySeconds -or [int]$State.crash_after_seconds -ne $CrashAfterSeconds -or [int]$State.min_rounds -ne $MinRounds) {
        throw "duration/workload contract changed; authority is frozen"
    }
}

function Save-State($State) {
    Write-AtomicJson $State.state_path $State
}

function Complete-InterruptedSegment($State) {
    if ($null -eq $State.active_segment) { return }
    $segment = $State.active_segment
    $segment | Add-Member -NotePropertyName ended_at -NotePropertyValue ([DateTime]::UtcNow.ToString("o")) -Force
    $segment | Add-Member -NotePropertyName status -NotePropertyValue "dirty" -Force
    $segment | Add-Member -NotePropertyName error -NotePropertyValue "previous driver process ended before the segment closed" -Force
    $State.segments = @($State.segments) + $segment
    $State.dirty_segments = @($State.dirty_segments) + $segment.segment_id
    $State.active_segment = $null
    Save-State $State
    throw "authority contains interrupted segment $($segment.segment_id); it is permanently non-resumable"
}

function Get-RunAttempts([string]$Endpoint, [string]$RunId) {
    return @((Invoke-Rw $Endpoint "list_attempts" @{ run_id = $RunId }).attempts)
}

function Get-RunLogs([string]$Endpoint, [string]$RunId, [int]$AttemptNo) {
    return (Invoke-Rw $Endpoint "logs" @{ run_id = $RunId; attempt_no = $AttemptNo; tail = 200 }).logs
}

function Assert-ExactlyTwoAttempts([string]$Endpoint, [string]$RunId) {
    $attempts = Get-RunAttempts $Endpoint $RunId
    if ($attempts.Count -ne 2) { throw "$RunId has $($attempts.Count) Attempts; expected 2" }
    if ([int]$attempts[0].attempt_no -ne 1 -or [int]$attempts[1].attempt_no -ne 2) {
        throw "$RunId Attempt numbering is not exactly 1,2"
    }
    return $attempts
}

function Assert-EventCount([string]$Endpoint, [string]$RunId, [string]$Kind, [int]$Expected) {
    $events = @((Invoke-Rw $Endpoint "list_run_events" @{ run_id = $RunId; limit = 200 }).events)
    $count = @($events | Where-Object { $_.kind -eq $Kind }).Count
    if ($count -ne $Expected) { throw "$RunId event $Kind count=$count expected=$Expected" }
}

function Invoke-Round($State, $Context, [int]$RoundNo) {
    $roundToken = "r{0:D3}" -f $RoundNo
    $localRun = "$($State.authority_id)-local-$roundToken"
    $slurmRun = "$($State.authority_id)-slurm-$roundToken"
    $localRequest = "$($State.authority_id)-local-retry-$roundToken"
    $slurmRequest = "$($State.authority_id)-slurm-retry-$roundToken"
    $localWorkspace = Join-Path $State.authority_root ("local/" + $roundToken)
    $localMarker = Join-Path $localWorkspace "attempt.marker"
    $localExec = Join-Path $localWorkspace "attempt2.exec"
    New-Item -ItemType Directory -Force -Path $localWorkspace | Out-Null

    $remoteWorkspace = "$($State.remote_root)/$($State.authority_id)/$roundToken"
    $remoteMarker = "$remoteWorkspace/attempt.marker"
    $remoteExec = "$remoteWorkspace/attempt2.exec"
    [void](Assert-Ssh ("mkdir -p -- " + (Quote-Sh $remoteWorkspace)) "create remote round workspace")

    $localMarkerQ = $localMarker.Replace("'", "''")
    $localExecQ = $localExec.Replace("'", "''")
    $localCommand = "if(-not (Test-Path -LiteralPath '$localMarkerQ')){[IO.File]::WriteAllText('$localMarkerQ','a1'); Write-Output 'R17_ENDURANCE_A1_FAIL'; exit 17}; [IO.File]::AppendAllText('$localExecQ',('a2' + [Environment]::NewLine)); Start-Sleep -Seconds $($State.delay_seconds); Write-Output 'R17_ENDURANCE_A2_OK'"
    $remoteCommand = "if [ ! -f $(Quote-Sh $remoteMarker) ]; then printf 'a1\n' > $(Quote-Sh $remoteMarker); echo R17_ENDURANCE_A1_FAIL; exit 17; fi; printf 'a2\n' >> $(Quote-Sh $remoteExec); sleep $($State.delay_seconds); echo R17_ENDURANCE_A2_OK"
    $resources = @{ time = $null; partition = $null; queue = $null; account = $null; cpus = $null; mem = $null; gpus = $null }
    $slurmResources = @{ time = (Format-SlurmTime ([int]$State.delay_seconds + 180)); partition = $null; queue = $null; account = $null; cpus = 1; mem = $null; gpus = $null }

    $slurmSpec = @{ run_id = $slurmRun; name = "R17 endurance Slurm $roundToken"; workspace = @{ host_alias = $State.slurm_host; cwd = $remoteWorkspace }; runner = "slurm"; command = $remoteCommand; resources = $slurmResources; continuation = $null }
    $localSpec = @{ run_id = $localRun; name = "R17 endurance Local $roundToken"; workspace = @{ host_alias = "local"; cwd = $localWorkspace }; runner = "process"; command = $localCommand; resources = $resources; continuation = $null }

    [void](Invoke-Rw $Context.endpoint "submit_run_v2" @{ spec = $slurmSpec })
    [void](Wait-RunStatus $Context.endpoint $slurmRun @("failed") 180)
    [void](Invoke-Rw $Context.endpoint "submit_run_v2" @{ spec = $localSpec })
    [void](Wait-RunStatus $Context.endpoint $localRun @("failed") 120)

    $slurmA1Logs = Get-RunLogs $Context.endpoint $slurmRun 1
    $localA1Logs = Get-RunLogs $Context.endpoint $localRun 1
    if ($slurmA1Logs.stdout -notmatch "R17_ENDURANCE_A1_FAIL" -or $localA1Logs.stdout -notmatch "R17_ENDURANCE_A1_FAIL") {
        throw "round $RoundNo Attempt-1 failure markers are missing"
    }

    $slurmRetry = @{ run_id = $slurmRun; expected_attempt_no = 1; request_id = $slurmRequest; resources = $null }
    $localRetry = @{ run_id = $localRun; expected_attempt_no = 1; request_id = $localRequest; resources = $null }
    $slurmRetryResult = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $slurmRetry }).run
    $slurmRunning = Wait-RunStatus $Context.endpoint $slurmRun @("running") 180
    $slurmJob = [string]$slurmRunning.job_id
    if ([string]::IsNullOrWhiteSpace($slurmJob)) { throw "round $RoundNo Slurm Attempt 2 has no JobID" }
    if ($slurmRetryResult.attempt_no -ne 2) { throw "round $RoundNo Slurm retry did not create Attempt 2" }

    $localRetryResult = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $localRetry }).run
    $localRunning = Wait-RunStatus $Context.endpoint $localRun @("running") 60
    $localHandle = [string]$localRunning.job_id
    if ([string]::IsNullOrWhiteSpace($localHandle)) { throw "round $RoundNo Local Attempt 2 has no handle" }
    if ($localRetryResult.attempt_no -ne 2) { throw "round $RoundNo Local retry did not create Attempt 2" }

    $activeStart = [DateTime]::UtcNow
    Start-Sleep -Seconds ([int]$State.crash_after_seconds)
    $beforeSupervisor = Get-PidFromHeartbeat $Context.data_dir "supervise.pid"
    $beforeServe = Get-PidFromHeartbeat $Context.data_dir "serve.pid"
    Stop-Resident $Context
    $after = Start-Resident $Context
    if ($after.supervisor_pid -eq $beforeSupervisor -or $after.serve_pid -eq $beforeServe) {
        throw "round $RoundNo resident PIDs did not change across crash/restart"
    }

    $slurmReplay = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $slurmRetry }).run
    $localReplay = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $localRetry }).run
    if ([string]$slurmReplay.job_id -ne $slurmJob) { throw "round $RoundNo Slurm replay changed JobID" }
    if ([string]$localReplay.job_id -ne $localHandle) { throw "round $RoundNo Local replay changed handle" }
    [void](Assert-ExactlyTwoAttempts $Context.endpoint $slurmRun)
    [void](Assert-ExactlyTwoAttempts $Context.endpoint $localRun)

    $slurmFinal = Wait-RunStatus $Context.endpoint $slurmRun @("succeeded") ([int]$State.delay_seconds + 300)
    $localFinal = Wait-RunStatus $Context.endpoint $localRun @("succeeded") ([int]$State.delay_seconds + 180)
    $activeEnd = [DateTime]::UtcNow
    if ([string]$slurmFinal.job_id -ne $slurmJob -or [string]$localFinal.job_id -ne $localHandle) {
        throw "round $RoundNo terminal handle identity changed"
    }

    $slurmTerminalReplay = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $slurmRetry }).run
    $localTerminalReplay = (Invoke-Rw $Context.endpoint "retry_run_v1" @{ retry = $localRetry }).run
    if ([string]$slurmTerminalReplay.job_id -ne $slurmJob -or [string]$localTerminalReplay.job_id -ne $localHandle) {
        throw "round $RoundNo terminal replay changed execution identity"
    }
    $slurmAttempts = Assert-ExactlyTwoAttempts $Context.endpoint $slurmRun
    $localAttempts = Assert-ExactlyTwoAttempts $Context.endpoint $localRun
    if ($slurmAttempts[0].status -ne "failed" -or $slurmAttempts[1].status -ne "succeeded" -or $localAttempts[0].status -ne "failed" -or $localAttempts[1].status -ne "succeeded") {
        throw "round $RoundNo final Attempt states are not failed -> succeeded"
    }

    $slurmA2Logs = Get-RunLogs $Context.endpoint $slurmRun 2
    $localA2Logs = Get-RunLogs $Context.endpoint $localRun 2
    if ($slurmA2Logs.stdout -notmatch "R17_ENDURANCE_A2_OK" -or $localA2Logs.stdout -notmatch "R17_ENDURANCE_A2_OK") {
        throw "round $RoundNo Attempt-2 success markers are missing"
    }
    if ((Get-RunLogs $Context.endpoint $slurmRun 1).stdout -notmatch "R17_ENDURANCE_A1_FAIL" -or (Get-RunLogs $Context.endpoint $localRun 1).stdout -notmatch "R17_ENDURANCE_A1_FAIL") {
        throw "round $RoundNo Attempt-1 logs were not preserved"
    }

    if (-not (Test-Path -LiteralPath $localExec)) { throw "round $RoundNo Local execution marker is missing" }
    $localExecCount = @(Get-Content -LiteralPath $localExec).Count
    if ($localExecCount -ne 1) { throw "round $RoundNo Local Attempt 2 executed $localExecCount times" }
    $remoteExecCountText = Assert-Ssh ("wc -l < " + (Quote-Sh $remoteExec)) "read remote execution count"
    $remoteExecCount = [int]$remoteExecCountText.Trim()
    if ($remoteExecCount -ne 1) { throw "round $RoundNo Slurm Attempt 2 executed $remoteExecCount times" }

    Assert-EventCount $Context.endpoint $slurmRun "scheduler_submitted" 2
    Assert-EventCount $Context.endpoint $localRun "local_process_started" 2
    $activeSeconds = ($activeEnd - $activeStart).TotalSeconds
    return [pscustomobject]@{
        round = $RoundNo
        started_at = $activeStart.ToString("o")
        ended_at = $activeEnd.ToString("o")
        active_seconds = [Math]::Round($activeSeconds, 3)
        local_run = $localRun
        local_attempt1_handle = [string]$localAttempts[0].job_id
        local_attempt2_handle = $localHandle
        slurm_run = $slurmRun
        slurm_attempt1_job = [string]$slurmAttempts[0].job_id
        slurm_attempt2_job = $slurmJob
        resident_before = "$beforeSupervisor/$beforeServe"
        resident_after = "$($after.supervisor_pid)/$($after.serve_pid)"
        resident_session = $after.session_id
        attempts = 2
        local_executions = $localExecCount
        slurm_executions = $remoteExecCount
        replay_ok = $true
        old_logs_ok = $true
    }
}

if ($TargetSeconds -le 0 -or $SegmentSeconds -le 0 -or $DelaySeconds -lt 10 -or $CrashAfterSeconds -lt 1 -or $CrashAfterSeconds -ge ($DelaySeconds - 5) -or $MinRounds -le 0) {
    throw "invalid endurance timing: target/segment/min-rounds must be positive, delay >=10s, and crash must leave at least 5s before completion"
}
if ($SlurmHost.Contains("`r") -or $SlurmHost.Contains("`n") -or $RemoteRoot.Contains("`r") -or $RemoteRoot.Contains("`n") -or -not $RemoteRoot.StartsWith('/')) {
    throw "Slurm host/remote root contract is invalid"
}
$RemoteRoot = $RemoteRoot.TrimEnd('/')

if ([string]::IsNullOrWhiteSpace($AuthorityId)) {
    if ($Resume -or $Status) { throw "-AuthorityId is required with -Resume or -Status" }
    $AuthorityId = "r17-retry-{0}-{1}" -f (Get-Date -Format "yyyyMMddHHmmss"), ([guid]::NewGuid().ToString('N').Substring(0, 8))
}
if ($AuthorityId -notmatch '^[A-Za-z0-9._-]{1,80}$') { throw "AuthorityId must be 1..80 ASCII letters/digits/._-" }
$AuthorityRoot = Join-Path $OutputRoot $AuthorityId
$StatePath = Join-Path $AuthorityRoot "state.json"

if ($PlanOnly) {
    [pscustomobject]@{
        authority_id = $AuthorityId
        authority_root = $AuthorityRoot
        target_seconds = $TargetSeconds
        segment_seconds = $SegmentSeconds
        delay_seconds = $DelaySeconds
        crash_after_seconds = $CrashAfterSeconds
        min_rounds = $MinRounds
        slurm_host = $SlurmHost
        remote_root = $RemoteRoot
        driver_sha256 = (Get-Sha256 $DriverPath)
    } | ConvertTo-Json -Depth 8
    exit 0
}

if ($Status) {
    $state = Read-State $StatePath
    $state | ConvertTo-Json -Depth 32
    exit 0
}

$state = $null
if ($Resume) {
    $state = Read-State $StatePath
    if ([int]$state.schema_version -ne $ScriptSchema) { throw "unsupported authority schema $($state.schema_version)" }
    if (@($state.dirty_segments).Count -gt 0) { throw "authority is dirty/non-resumable: $(@($state.dirty_segments) -join ', ')" }
    Assert-AuthorityIdentity $state
    Complete-InterruptedSegment $state
    if ($state.qualified) {
        Write-Host "R17_ENDURANCE_ALREADY_QUALIFIED authority=$($state.authority_id) clean_seconds=$($state.cumulative_clean_seconds) rounds=$(@($state.rounds).Count)"
        exit 0
    }
} else {
    if (Test-Path -LiteralPath $AuthorityRoot) { throw "authority already exists: $AuthorityRoot" }
    Assert-CleanWorktree
    $sourceExe = (Resolve-Path (Join-Path $RepoRoot $RunwatchExe)).Path
    New-Item -ItemType Directory -Force -Path (Join-Path $AuthorityRoot "runtime"), (Join-Path $AuthorityRoot "data") | Out-Null
    $frozenExe = Join-Path $AuthorityRoot "runtime/runwatch.exe"
    Copy-Item -LiteralPath $sourceExe -Destination $frozenExe
    $endpointToken = (Get-Sha256 $frozenExe).Substring(0, 8) + "-" + $AuthorityId.Substring(0, [Math]::Min(20, $AuthorityId.Length))
    $state = [pscustomobject]@{
        schema_version = $ScriptSchema
        authority_id = $AuthorityId
        authority_root = $AuthorityRoot
        state_path = $StatePath
        created_at = [DateTime]::UtcNow.ToString("o")
        git_commit = (Invoke-Git @("rev-parse", "HEAD"))
        driver_sha256 = (Get-Sha256 $DriverPath)
        runtime_path = $frozenExe
        runtime_sha256 = (Get-Sha256 $frozenExe)
        source_runtime_sha256 = (Get-Sha256 $sourceExe)
        data_dir = (Join-Path $AuthorityRoot "data")
        endpoint = "\\.\pipe\runwatch-r17-endurance-$endpointToken"
        task_name = "Runwatch-R17-Endurance-$($endpointToken.Substring(0, [Math]::Min(24, $endpointToken.Length)))"
        slurm_host = $SlurmHost
        remote_root = $RemoteRoot.TrimEnd('/')
        target_seconds = $TargetSeconds
        delay_seconds = $DelaySeconds
        crash_after_seconds = $CrashAfterSeconds
        min_rounds = $MinRounds
        cumulative_clean_seconds = 0.0
        rounds = @()
        segments = @()
        dirty_segments = @()
        active_segment = $null
        qualified = $false
    }
    Save-State $state
}

$segmentNo = @($state.segments).Count + 1
$segmentId = "segment-{0:D3}" -f $segmentNo
$state.active_segment = [pscustomobject]@{
    segment_id = $segmentId
    started_at = [DateTime]::UtcNow.ToString("o")
    round_start = @($state.rounds).Count + 1
    completed_rounds = @()
    active_seconds = 0.0
}
Save-State $state
$context = New-ResidentContext $state $segmentId
$segmentClean = $false

try {
    Write-ResidentTask $context
    $resident = Start-Resident $context
    Write-Host "R17_ENDURANCE_SEGMENT_START authority=$($state.authority_id) segment=$segmentId session=$($resident.session_id) clean_seconds=$($state.cumulative_clean_seconds)"

    while (([double]$state.cumulative_clean_seconds + [double]$state.active_segment.active_seconds) -lt [double]$state.target_seconds -and [double]$state.active_segment.active_seconds -lt [double]$SegmentSeconds) {
        $roundNo = @($state.rounds).Count + @($state.active_segment.completed_rounds).Count + 1
        $round = Invoke-Round $state $context $roundNo
        $state.active_segment.completed_rounds = @($state.active_segment.completed_rounds) + $round
        $state.active_segment.active_seconds = [Math]::Round(([double]$state.active_segment.active_seconds + [double]$round.active_seconds), 3)
        Save-State $state
        Write-Host "R17_ENDURANCE_ROUND_OK authority=$($state.authority_id) round=$roundNo active_seconds=$($round.active_seconds) segment_active=$($state.active_segment.active_seconds)"
    }

    $closed = $state.active_segment
    $closed | Add-Member -NotePropertyName ended_at -NotePropertyValue ([DateTime]::UtcNow.ToString("o")) -Force
    $closed | Add-Member -NotePropertyName status -NotePropertyValue "clean" -Force
    $state.rounds = @($state.rounds) + @($closed.completed_rounds)
    $state.cumulative_clean_seconds = [Math]::Round(([double]$state.cumulative_clean_seconds + [double]$closed.active_seconds), 3)
    $state.segments = @($state.segments) + $closed
    $state.active_segment = $null
    $state.qualified = ([double]$state.cumulative_clean_seconds -ge [double]$state.target_seconds -and @($state.rounds).Count -ge [int]$state.min_rounds -and @($state.dirty_segments).Count -eq 0)
    Save-State $state
    $segmentClean = $true
    Write-Host "R17_ENDURANCE_SEGMENT_OK authority=$($state.authority_id) segment=$segmentId clean_seconds=$($state.cumulative_clean_seconds) rounds=$(@($state.rounds).Count) qualified=$($state.qualified)"
} catch {
    $message = $_.Exception.Message
    if ($null -ne $state.active_segment) {
        $dirty = $state.active_segment
        $dirty | Add-Member -NotePropertyName ended_at -NotePropertyValue ([DateTime]::UtcNow.ToString("o")) -Force
        $dirty | Add-Member -NotePropertyName status -NotePropertyValue "dirty" -Force
        $dirty | Add-Member -NotePropertyName error -NotePropertyValue $message -Force
        $state.segments = @($state.segments) + $dirty
        $state.dirty_segments = @($state.dirty_segments) + $dirty.segment_id
        $state.active_segment = $null
        Save-State $state
    }
    Write-Host "R17_ENDURANCE_SEGMENT_DIRTY authority=$($state.authority_id) segment=$segmentId error=$message"
    throw
} finally {
    Remove-ResidentTask $context
}

if ($segmentClean -and $state.qualified) {
    Write-Host "R17_ENDURANCE_QUALIFIED authority=$($state.authority_id) clean_seconds=$($state.cumulative_clean_seconds) rounds=$(@($state.rounds).Count) dirty_segments=0"
} elseif ($segmentClean) {
    Write-Host "R17_ENDURANCE_RESUME_REQUIRED authority=$($state.authority_id) clean_seconds=$($state.cumulative_clean_seconds) rounds=$(@($state.rounds).Count) target=$($state.target_seconds)"
}
