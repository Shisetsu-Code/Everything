#Requires -Version 7.0
<#
.SYNOPSIS
Compares two Windows es.exe builds against one real, unchanged filesystem tree.
.DESCRIPTION
Uses the existing CLI index/search/stats commands, so it also supports builds
without benchmark-filesystem. Each version/repetition gets its own database.
Index timings include process startup, configuration, scanning, SQLite work and
shutdown. The GUI engine and native watcher are not measured by this script.
No cache eviction or fixed performance threshold is applied. Failed runs retain
their workspace and a bounded JSON diagnostic report for investigation.
.EXAMPLE
pwsh -File tools/benchmark-indexing.ps1 -BaselineExe .\old\es.exe `
    -CandidateExe .\new\es.exe -OutputFile .\indexing-comparison.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $BaselineExe,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $CandidateExe,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $OutputFile,

    [ValidateRange(1000, 500000)]
    [int] $Files = 100000,

    [ValidateRange(1, 30)]
    [int] $Repetitions = 3,

    [ValidateRange(1, 86400)]
    [int] $TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $IsWindows) {
    throw 'This comparison requires Windows and PowerShell 7 or newer.'
}

$utf8 = [System.Text.UTF8Encoding]::new($false)
$diagnosticLimit = 8192
$commands = [System.Collections.Generic.List[object]]::new()
$samples = [System.Collections.Generic.List[object]]::new()
$workspace = $null
$failure = $null

function Limit-Diagnostic {
    param([AllowNull()][string] $Text)
    if ($null -eq $Text) { return '' }
    if ($Text.Length -le $diagnosticLimit) { return $Text }
    # Keep both the command summary and its most recent diagnostics.
    $marker = "`n... [truncated] ...`n"
    $head = [int] [Math]::Floor(($diagnosticLimit - $marker.Length) / 2.0)
    $tail = $diagnosticLimit - $marker.Length - $head
    return $Text.Substring(0, $head) + $marker + $Text.Substring($Text.Length - $tail)
}

function Invoke-EsProcess {
    param(
        [string] $Executable,
        [string[]] $CommandArguments,
        [string] $Label
    )

    $record = [ordered]@{
        id = $commands.Count + 1
        label = $Label
        executable = $Executable
        arguments = @($CommandArguments)
        started_utc = [DateTime]::UtcNow.ToString('o')
        seconds = 0.0
        exit_code = $null
        timed_out = $false
        process_terminated = $false
        streams_drained = $false
        stdout = ''
        stderr = ''
        stdout_truncated = $false
        stderr_truncated = $false
        error = $null
    }
    $commands.Add($record)
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $utf8
    $startInfo.StandardErrorEncoding = $utf8
    foreach ($argument in $CommandArguments) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $clock = [System.Diagnostics.Stopwatch]::new()
    $started = $false
    $stdoutTask = $null
    $stderrTask = $null

    try {
        $clock.Start()
        $started = $process.Start()
        if (-not $started) { throw 'Process.Start returned false.' }
        # Both pipes must be consumed before waiting, including on timeout.
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $record.timed_out = $true
            $record.error = "Exceeded the $TimeoutSeconds second process timeout."
            $process.Kill($true)
            if (-not $process.WaitForExit(10000)) {
                throw 'Process tree termination did not finish within 10 seconds.'
            }
        }
        $clock.Stop()
        $record.process_terminated = $process.HasExited
        if ($record.process_terminated) { $record.exit_code = $process.ExitCode }
        $readers = [System.Threading.Tasks.Task[]] @($stdoutTask, $stderrTask)
        if (-not [System.Threading.Tasks.Task]::WaitAll($readers, 10000)) {
            throw 'Output streams did not close within 10 seconds after process exit.'
        }
        $record.streams_drained = $true
    }
    catch {
        $detail = Limit-Diagnostic -Text $_.Exception.Message
        if ($null -eq $record.error) { $record.error = $detail }
        else { $record.error = Limit-Diagnostic -Text ($record.error + ' ' + $detail) }
    }
    finally {
        if ($started -and -not $process.HasExited) {
            try {
                $process.Kill($true)
                $record.process_terminated = $process.WaitForExit(10000)
            }
            catch {
                $record.error = Limit-Diagnostic -Text (
                    [string] $record.error + ' Shutdown: ' + $_.Exception.Message
                )
            }
        }
        if ($started -and $process.HasExited) {
            $record.process_terminated = $true
            $record.exit_code = $process.ExitCode
        }
        $clock.Stop()
        $record.seconds = $clock.Elapsed.TotalSeconds
        # Never block on a reader's Result after an unconfirmed shutdown.
        if ($null -ne $stdoutTask -and $stdoutTask.IsCompletedSuccessfully) {
            $text = $stdoutTask.GetAwaiter().GetResult()
            $record.stdout_truncated = $text.Length -gt $diagnosticLimit
            $record.stdout = Limit-Diagnostic -Text $text
        }
        if ($null -ne $stderrTask -and $stderrTask.IsCompletedSuccessfully) {
            $text = $stderrTask.GetAwaiter().GetResult()
            $record.stderr_truncated = $text.Length -gt $diagnosticLimit
            $record.stderr = Limit-Diagnostic -Text $text
        }
        $process.Dispose()
    }
    return $record
}

function Assert-CommandSucceeded {
    param($Command)
    if ($Command.timed_out -or $null -ne $Command.error -or
        -not $Command.process_terminated -or -not $Command.streams_drained -or
        $Command.exit_code -ne 0) {
        throw "Command $($Command.id) ($($Command.label)) failed; see its bounded stdout/stderr in the JSON report. Error: $($Command.error)"
    }
}

function Write-Report {
    $json = ConvertTo-Json -InputObject $report -Depth 30
    [System.IO.File]::WriteAllText($outputPath, $json + "`n", $utf8)
}

function Get-Median {
    param([double[]] $Values)
    if ($Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    $middle = [int] [Math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2 -eq 1) { return [double] $sorted[$middle] }
    return ([double] $sorted[$middle - 1] + [double] $sorted[$middle]) / 2.0
}

function New-IndexPhase {
    return [ordered]@{
        seconds = $null
        files_per_second = $null
        entries_per_second = $null
        expected_entries = $expectedEntries
        reported_entries = $null
        access_errors = $null
        command_id = $null
        database_bytes = $null
        checks = [System.Collections.Generic.List[object]]::new()
        integrity = $null
    }
}

function Set-IndexResult {
    param($Command, $Phase)
    $Phase.command_id = $Command.id
    $Phase.seconds = $Command.seconds
    Write-Report
    Assert-CommandSucceeded -Command $Command
    $indexSummary = [regex]::Match($Command.stdout, '(?m)^\s*([0-9]+) entradas indexadas;\s*([0-9]+) errores de acceso;')
    if (-not $indexSummary.Success) { throw "$($Command.label) returned an unrecognized index summary." }
    $Phase.reported_entries = [long] $indexSummary.Groups[1].Value
    $Phase.access_errors = [long] $indexSummary.Groups[2].Value
    Write-Report
    if ($Phase.reported_entries -ne $expectedEntries -or $Phase.access_errors -ne 0) {
        throw "$($Command.label) reported $($Phase.reported_entries) entries and $($Phase.access_errors) access errors; expected $expectedEntries and zero."
    }
    $Phase.files_per_second = $Files / [Math]::Max($Command.seconds, [double]::Epsilon)
    $Phase.entries_per_second = $expectedEntries / [Math]::Max($Command.seconds, [double]::Epsilon)
}

function Add-Verification {
    param(
        [string] $Executable,
        [string] $DataDirectory,
        [string] $Label,
        $Phase
    )

    foreach ($check in @(
        @{ query = 'file:'; expected = [long] $Files },
        @{ query = 'folder:'; expected = $expectedDirectories },
        @{ query = 'ext:pdf'; expected = [long] [Math]::Ceiling($Files / 4.0) },
        @{ query = 'informe_á_日本_00000000'; expected = 1L }
    )) {
        $command = Invoke-EsProcess -Executable $Executable -Label "$Label search $($check.query)" `
            -CommandArguments @('--data-dir', $DataDirectory, 'search', $check.query, '--json', '--limit', '1')
        Write-Report
        Assert-CommandSucceeded -Command $command
        if ($command.stdout_truncated) { throw 'Search JSON exceeded the diagnostic size limit.' }
        $page = ConvertFrom-Json -InputObject $command.stdout -AsHashtable
        if (-not $page.ContainsKey('total') -or $null -eq $page.total) {
            throw "Search response does not contain a total: command $($command.id)."
        }
        $actual = [long] $page.total
        $Phase.checks.Add([ordered]@{
            query = $check.query
            expected = [long] $check.expected
            actual = $actual
            matches = $actual -eq $check.expected
            command_id = $command.id
        })
        Write-Report
        if ($actual -ne $check.expected) {
            throw "$Label search '$($check.query)' returned $actual; expected $($check.expected)."
        }
    }
    $command = Invoke-EsProcess -Executable $Executable -Label "$Label stats --check" `
        -CommandArguments @('--data-dir', $DataDirectory, 'stats', '--check')
    Write-Report
    Assert-CommandSucceeded -Command $command
    if ($command.stdout_truncated) { throw 'Stats JSON exceeded the diagnostic size limit.' }
    $stats = ConvertFrom-Json -InputObject $command.stdout -AsHashtable
    foreach ($key in @('entries', 'database_bytes', 'integrity_checked')) {
        if (-not $stats.ContainsKey($key) -or $null -eq $stats[$key]) {
            throw "Stats response does not contain '$key': command $($command.id)."
        }
    }
    $Phase.integrity = [ordered]@{
        expected_entries = $expectedEntries
        actual_entries = [long] $stats.entries
        integrity_checked = $stats.integrity_checked
        command_id = $command.id
    }
    $Phase.database_bytes = [long] $stats.database_bytes
    Write-Report
    if ($stats.integrity_checked -isnot [bool] -or -not $stats.integrity_checked -or
        [long] $stats.entries -ne $expectedEntries -or $Phase.database_bytes -le 0) {
        throw "$Label failed the integrity, total-entry, or database-size check."
    }
}

$baselineItem = Get-Item -LiteralPath $BaselineExe
$candidateItem = Get-Item -LiteralPath $CandidateExe
if ($baselineItem -isnot [System.IO.FileInfo] -or $candidateItem -isnot [System.IO.FileInfo]) {
    throw 'BaselineExe and CandidateExe must name existing executable files.'
}
$outputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputFile)
foreach ($executablePath in @($baselineItem.FullName, $candidateItem.FullName)) {
    if ([string]::Equals($outputPath, $executablePath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'OutputFile must not overwrite either executable.'
    }
}
[void] [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($outputPath))
$expectedDirectories = 1L + [long] [Math]::Ceiling($Files / 1000.0) +
    [long] [Math]::Ceiling($Files / 100.0)
$expectedEntries = [long] $Files + $expectedDirectories
$variants = [ordered]@{
    baseline = [ordered]@{ path = $baselineItem.FullName; version = $null; sha256 = $null; size_bytes = $baselineItem.Length }
    candidate = [ordered]@{ path = $candidateItem.FullName; version = $null; sha256 = $null; size_bytes = $candidateItem.Length }
}
$report = [ordered]@{
    schema_version = 1
    kind = 'windows_cli_indexing_comparison_same_filesystem_tree'
    status = 'running'
    started_utc = [DateTime]::UtcNow.ToString('o')
    completed_utc = $null
    environment = [ordered]@{
        os = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        os_version = [Environment]::OSVersion.Version.ToString()
        os_architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        process_architecture = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
        logical_processors = [Environment]::ProcessorCount
        processor_identifier = [Environment]::GetEnvironmentVariable('PROCESSOR_IDENTIFIER')
        powershell_version = $PSVersionTable.PSVersion.ToString()
        dotnet_runtime = [System.Runtime.InteropServices.RuntimeInformation]::FrameworkDescription
        temp_volume = $null
        temp_filesystem = $null
        temp_available_bytes_before_fixture = $null
    }
    parameters = [ordered]@{ files = $Files; repetitions = $Repetitions; timeout_seconds_per_process = $TimeoutSeconds }
    methodology = [ordered]@{
        pipeline = 'CLI index, without the GUI engine or native watcher'
        timing = 'Stopwatch from process start through exit; verification and fixture creation excluded'
        cache = 'One newly written tree is shared by all runs; caches are not flushed. Initial indexing has a fresh database, not a cold filesystem cache. Unchanged rescans reuse that version/repetition database.'
        order = 'Odd repetitions: baseline then candidate. Even repetitions: candidate then baseline.'
        speed_threshold = $null
        diagnostic_characters_per_stream = $diagnosticLimit
    }
    executables = $variants
    fixture = [ordered]@{
        root = $null
        files = $Files
        directories = $expectedDirectories
        entries = $expectedEntries
        bytes_per_file = $null
        creation_seconds = $null
    }
    samples = $samples
    summary = $null
    compatibility = $null
    commands = $commands
    failure = $null
    cleanup = [ordered]@{ workspace = $null; removed = $false; retained_reason = $null }
}

try {
    Write-Report
    foreach ($variant in @('baseline', 'candidate')) {
        $executable = $variants[$variant]
        $executable.sha256 = (Get-FileHash -LiteralPath $executable.path -Algorithm SHA256).Hash.ToLowerInvariant()
        $command = Invoke-EsProcess -Executable $executable.path -CommandArguments @('--version') -Label "$variant version"
        Write-Report
        Assert-CommandSucceeded -Command $command
        $executable.version = $command.stdout.Trim()
        if ([string]::IsNullOrWhiteSpace($executable.version)) { throw "$variant --version returned no version." }
    }

    $workspace = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), 'everything-indexing-' + [Guid]::NewGuid().ToString('N'))
    $fixtureRoot = [System.IO.Path]::Combine($workspace, 'files')
    $report.fixture.root = $fixtureRoot
    $report.cleanup.workspace = $workspace
    [void] [System.IO.Directory]::CreateDirectory($fixtureRoot)
    $drive = [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($workspace))
    $report.environment.temp_volume = $drive.Name
    $report.environment.temp_filesystem = $drive.DriveFormat
    $report.environment.temp_available_bytes_before_fixture = $drive.AvailableFreeSpace
    $content = $utf8.GetBytes("Everything filesystem benchmark.`n")
    $report.fixture.bytes_per_file = $content.Length
    $fixtureClock = [System.Diagnostics.Stopwatch]::StartNew()
    $extensions = @('pdf', 'rs', 'txt', 'png')
    for ($index = 0; $index -lt $Files; $index++) {
        $groupIndex = [int] [Math]::Floor($index / 1000.0)
        $bucketIndex = [int] [Math]::Floor(($index % 1000) / 100.0)
        $group = [System.IO.Path]::Combine($fixtureRoot, ('group_{0:D4}' -f $groupIndex))
        $bucket = [System.IO.Path]::Combine($group, ('bucket_{0:D2}' -f $bucketIndex))
        if ($index % 100 -eq 0) { [void] [System.IO.Directory]::CreateDirectory($bucket) }
        $stem = if ($index % 10 -eq 0) { 'informe_á_日本' } else { 'document' }
        $name = '{0}_{1:D8}.{2}' -f $stem, $index, $extensions[$index % 4]
        [System.IO.File]::WriteAllBytes([System.IO.Path]::Combine($bucket, $name), $content)
    }
    $fixtureClock.Stop()
    $report.fixture.creation_seconds = $fixtureClock.Elapsed.TotalSeconds
    Write-Report

    for ($repetition = 1; $repetition -le $Repetitions; $repetition++) {
        $order = if ($repetition % 2 -eq 1) { @('baseline', 'candidate') } else { @('candidate', 'baseline') }
        for ($position = 0; $position -lt $order.Count; $position++) {
            $variant = $order[$position]
            $executable = $variants[$variant].path
            $dataDirectory = [System.IO.Path]::Combine($workspace, 'indexes', ('repeat_{0:D2}_{1}' -f $repetition, $variant))
            $sample = [ordered]@{
                repetition = $repetition
                variant = $variant
                order_position = $position + 1
                execution_order = @($order)
                data_directory = $dataDirectory
                status = 'running'
                initial_index = $null
                unchanged_rescan = $null
            }
            $samples.Add($sample)
            foreach ($phaseName in @('initial_index', 'unchanged_rescan')) {
                $phase = New-IndexPhase
                $sample[$phaseName] = $phase
                Write-Report
                $label = "repetition $repetition $variant $phaseName"
                Write-Host $label
                $command = Invoke-EsProcess -Executable $executable -Label $label `
                    -CommandArguments @('--data-dir', $dataDirectory, '--root', $fixtureRoot, 'index')
                Set-IndexResult -Command $command -Phase $phase
                Add-Verification -Executable $executable -DataDirectory $dataDirectory -Label $label -Phase $phase
            }
            $sample.status = 'passed'
            Write-Report
        }
    }

    $summary = [ordered]@{}
    foreach ($phaseName in @('initial_index', 'unchanged_rescan')) {
        $baselineSamples = @($samples | Where-Object { $_.variant -eq 'baseline' })
        $candidateSamples = @($samples | Where-Object { $_.variant -eq 'candidate' })
        $baselineMedian = Get-Median -Values @($baselineSamples | ForEach-Object { $_[$phaseName].seconds })
        $candidateMedian = Get-Median -Values @($candidateSamples | ForEach-Object { $_[$phaseName].seconds })
        $summary[$phaseName] = [ordered]@{
            baseline_median_seconds = $baselineMedian
            candidate_median_seconds = $candidateMedian
            baseline_files_per_second_at_median = $Files / $baselineMedian
            candidate_files_per_second_at_median = $Files / $candidateMedian
            candidate_time_reduction_percent = 100.0 * ($baselineMedian - $candidateMedian) / $baselineMedian
            candidate_speedup_ratio = $baselineMedian / $candidateMedian
            baseline_median_database_bytes = Get-Median -Values @($baselineSamples | ForEach-Object { $_[$phaseName].database_bytes })
            candidate_median_database_bytes = Get-Median -Values @($candidateSamples | ForEach-Object { $_[$phaseName].database_bytes })
            baseline_samples = $baselineSamples.Count
            candidate_samples = $candidateSamples.Count
        }
    }
    $report.summary = $summary

    # Exercise the actual upgrade path without changing the measured samples:
    # open an existing baseline database with the candidate, then reconcile it.
    $baselineSample = @($samples | Where-Object { $_.variant -eq 'baseline' })[-1]
    $compatibilityPhase = New-IndexPhase
    $report.compatibility = [ordered]@{
        status = 'running'
        direction = 'candidate opens and reindexes an existing baseline database'
        excluded_from_performance_summary = $true
        baseline_repetition = $baselineSample.repetition
        data_directory = $baselineSample.data_directory
        database_bytes_before = $baselineSample.unchanged_rescan.database_bytes
        result = $compatibilityPhase
    }
    Write-Report
    Write-Host 'compatibility: candidate opens the baseline database'
    $command = Invoke-EsProcess -Executable $variants.candidate.path -Label 'compatibility candidate on baseline database' `
        -CommandArguments @('--data-dir', $baselineSample.data_directory, '--root', $fixtureRoot, 'index')
    Set-IndexResult -Command $command -Phase $compatibilityPhase
    Add-Verification -Executable $variants.candidate.path -DataDirectory $baselineSample.data_directory `
        -Label 'compatibility' -Phase $compatibilityPhase
    $report.compatibility.status = 'passed'
    $report.status = 'passed'
}
catch {
    $failure = Limit-Diagnostic -Text $_.Exception.Message
    $report.failure = $failure
    $report.status = 'failed'
    if ($null -ne $report.compatibility -and $report.compatibility.status -eq 'running') {
        $report.compatibility.status = 'failed'
    }
}
finally {
    $unsafeCommands = @($commands | Where-Object {
        $_.timed_out -or -not $_.process_terminated -or -not $_.streams_drained
    })
    if ($null -ne $workspace -and [System.IO.Directory]::Exists($workspace)) {
        if ($report.status -eq 'passed' -and $unsafeCommands.Count -eq 0) {
            try {
                [System.IO.Directory]::Delete($workspace, $true)
                $report.cleanup.removed = $true
            }
            catch {
                $failure = Limit-Diagnostic -Text ('Cleanup failed: ' + $_.Exception.Message)
                $report.failure = $failure
                $report.status = 'failed'
                $report.cleanup.retained_reason = $failure
            }
        }
        else {
            # Kill(true) and the parent's exit do not prove that every possible
            # descendant has exited. Keep failed runs rather than race cleanup.
            $report.cleanup.retained_reason = 'Run failed or process/stream termination was not confirmed; retained for diagnosis.'
        }
    }
    $report.completed_utc = [DateTime]::UtcNow.ToString('o')
    Write-Report
}

if ($null -ne $failure) {
    throw "Indexing comparison failed: $failure Report: $outputPath Workspace: $workspace"
}
Write-Host "Verified comparison saved to $outputPath"
