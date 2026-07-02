param(
    [Parameter(Mandatory = $true)]
    [string]$Event
)

$ErrorActionPreference = 'Stop'

$timeoutMs = switch ($Event) {
    'SessionStart' { 8000 }
    'Stop' { 8000 }
    'PreCompact' { 8000 }
    default { 5000 }
}

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = Join-Path $env:USERPROFILE '.cargo\bin\sentinel.exe'
$psi.Arguments = "hook --event $Event"
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true

$proc = [System.Diagnostics.Process]::new()
$proc.StartInfo = $psi

$stdoutBuilder = [System.Text.StringBuilder]::new()
$stderrBuilder = [System.Text.StringBuilder]::new()

$proc.add_OutputDataReceived({
    param($sender, $args)
    if ($null -ne $args.Data) {
        [void]$stdoutBuilder.AppendLine($args.Data)
    }
})

$proc.add_ErrorDataReceived({
    param($sender, $args)
    if ($null -ne $args.Data) {
        [void]$stderrBuilder.AppendLine($args.Data)
    }
})

if (-not $proc.Start()) {
    throw 'Failed to start sentinel hook process.'
}

$proc.BeginOutputReadLine()
$proc.BeginErrorReadLine()

if ($proc.WaitForExit($timeoutMs)) {
    $proc.WaitForExit()
    $stderr = $stderrBuilder.ToString()
    $stdout = $stdoutBuilder.ToString()

    if (-not [string]::IsNullOrWhiteSpace($stderr)) {
        [Console]::Error.Write($stderr)
    }

    if ([string]::IsNullOrWhiteSpace($stdout)) {
        [Console]::Out.Write('{}')
    } else {
        [Console]::Out.Write($stdout.TrimEnd("`r", "`n"))
    }

    exit $proc.ExitCode
}

[Console]::Error.WriteLine("[sentinel-hook-timeout] $Event exceeded ${timeoutMs}ms; killing sentinel process tree.")

try {
    & taskkill /PID $proc.Id /T /F *> $null
} catch {
    try {
        $proc.Kill($true)
    } catch {
    }
}

[Console]::Out.Write('{}')
exit 0
