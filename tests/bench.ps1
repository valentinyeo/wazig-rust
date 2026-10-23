$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'Benchmarks run only on GitHub Actions' }
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class NativeBench {
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, UIntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
}
'@
$utf8 = [Text.UTF8Encoding]::new($false)
$evidence = Join-Path $PWD 'bench-evidence'
New-Item $evidence -ItemType Directory -Force | Out-Null
$root = Join-Path $env:RUNNER_TEMP ('wazig-rust-bench-' + [Guid]::NewGuid())
New-Item $root -ItemType Directory | Out-Null
$portFile = Join-Path $root 'port.txt'
$server = Start-Process python -ArgumentList @((Join-Path $PWD 'tests/fake_backend.py'), '--port-file', $portFile) -PassThru -RedirectStandardError (Join-Path $evidence 'backend-errors.txt')
$script:rust = $null
$script:zig = $null

function Wait-For([scriptblock]$Check, [string]$What, [int]$Seconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $result = & $Check
        if ($result) { return $result }
        Start-Sleep -Milliseconds 20
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $What"
}
function Pipe([string]$Name, [string]$Command, [int]$ConnectTimeout = 200) {
    $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $Name, [IO.Pipes.PipeDirection]::InOut)
    try {
        $pipe.Connect($ConnectTimeout)
        $writer = [IO.StreamWriter]::new($pipe, $utf8, 4096, $true)
        $writer.WriteLine($Command)
        $writer.Flush()
        $reader = [IO.StreamReader]::new($pipe, $utf8, $false, 4096, $true)
        $pending = $reader.ReadLineAsync()
        if (-not $pending.Wait(15000)) { throw "Pipe response timed out: $Name" }
        return ($pending.Result | ConvertFrom-Json)
    } finally { $pipe.Dispose() }
}
function Rust-State($App) { Pipe "WazigRust-$($App.Id)" 'state' }
function Zig-Control($Session, [hashtable]$Command) {
    $response = Pipe "wazig-control-$($Session.user)" ($Command | ConvertTo-Json -Compress)
    if (-not $response.ok) { throw "Zig control error: $($response.error)" }
    return $response.result
}
function Start-App([string]$Exe, [string]$Kind) {
    $id = [Guid]::NewGuid().ToString()
    $dir = Join-Path $root "$Kind-$id"
    $local = Join-Path $dir 'local'
    $profile = Join-Path $dir 'profile'
    New-Item $local, $profile -ItemType Directory -Force | Out-Null
    $info = [Diagnostics.ProcessStartInfo]::new($Exe)
    $info.UseShellExecute = $false
    $info.Environment['LOCALAPPDATA'] = $local
    $info.Environment['APPDATA'] = $local
    $info.Environment['USERPROFILE'] = $profile
    $info.Environment['USERNAME'] = "bench-$id"
    $info.Environment['WAZIG_HUB_URL'] = $script:hub
    $info.Environment['WAZIG_HUB_TOKEN'] = 'fixture-key-not-a-secret'
    foreach ($key in @('DEEPGRAM_API_KEY','OPENROUTER_API_KEY','WA_BRIDGE_KEY','GH_TOKEN','GITHUB_TOKEN')) { $null = $info.Environment.Remove($key) }
    if ($Kind -eq 'zig') {
        $info.ArgumentList.Add('--verify-fixture')
        $info.Environment['WAZIG_VERIFY_ROOT'] = $dir
        $info.Environment['WAZIG_VERIFY_ID'] = $id
    }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $app = [Diagnostics.Process]::Start($info)
    return @{ app = $app; user = "bench-$id"; root = $dir; clock = $clock }
}
function Memory($App) {
    $App.Refresh()
    if ($App.HasExited) { throw 'Application exited during benchmark' }
    return @{ working_set = [Math]::Round($App.WorkingSet64 / 1MB, 2); private_bytes = [Math]::Round($App.PrivateMemorySize64 / 1MB, 2) }
}
function Median($Values) {
    $sorted = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2) { return [double]$sorted[$middle] }
    return ([double]$sorted[$middle-1] + [double]$sorted[$middle]) / 2
}
function Stop-App($App) {
    if ($null -ne $App -and -not $App.HasExited) { $App.Kill(); $App.WaitForExit() }
}
try {
    $null = Wait-For { Test-Path $portFile } 'fake backend port'
    $script:hub = 'http://127.0.0.1:' + (Get-Content $portFile -Raw).Trim()
    $rustExe = (Resolve-Path 'target/release/wazig-rust.exe').Path
    $latest = gh release view -R valentinyeo/wazig --json tagName,assets | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cannot find latest Zig release' }
    $asset = $latest.assets | Where-Object { $_.name -match '^Messages-v[0-9.]+\.zip$' } | Select-Object -First 1
    if (-not $asset) { throw 'Latest Zig release has no x64 portable ZIP' }
    Invoke-WebRequest $asset.url -OutFile (Join-Path $root 'zig.zip')
    Expand-Archive (Join-Path $root 'zig.zip') (Join-Path $root 'zig')
    $zigExe = (Get-ChildItem (Join-Path $root 'zig') -Filter Messages.exe -Recurse | Select-Object -First 1).FullName
    if (-not $zigExe) { throw 'Messages.exe missing from published Zig ZIP' }
    $rustStartup = @(); $rustObserved = @(); $zigStartup = @()
    # Each process gets an empty private profile: includes font extraction and media/cache setup.
    for ($i = 0; $i -lt 10; $i++) {
        $r = Start-App $rustExe 'rust'
        $script:rust = $r.app
        $state = Wait-For {
            if ($r.app.HasExited) { throw "Rust exited: $($r.app.ExitCode)" }
            try { $s = Rust-State $r.app; if ($s.chats -eq 200 -and $null -ne $s.first_chat_painted_unix_ms) { return $s } } catch [TimeoutException] {}
        } 'Rust first painted 200-chat list'
        $rustObserved += $r.clock.Elapsed.TotalMilliseconds
        $startEpoch = ([DateTimeOffset]$r.app.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
        $rustStartup += ([double]$state.first_chat_painted_unix_ms - $startEpoch)
        $null = Wait-For { $s = Rust-State $r.app; if ($s.messages_visible -gt 0) { $s } } 'Rust opened initial chat'
        if ($i -lt 9) { Stop-App $r.app; $script:rust = $null }
        $z = Start-App $zigExe 'zig'
        $script:zig = $z.app
        $null = Wait-For {
            if ($z.app.HasExited) { throw "Zig exited: $($z.app.ExitCode)" }
            try { $s = Zig-Control $z @{cmd='status'}; if ($s.chat_count -eq 200 -and $s.paints.list -gt 0) { return $s } } catch [TimeoutException] {}
        } 'Zig 200 chats and list paint'
        $zigStartup += $z.clock.Elapsed.TotalMilliseconds
        if ($i -lt 9) { Stop-App $z.app; $script:zig = $null }
    }
    # Both retained final processes are loaded against the very same server/fixtures.
    $jid = 'personal:15550000001@s.whatsapp.net'
    $selected = Pipe "WazigRust-$($rust.Id)" "select $jid"
    if (-not $selected.ok) { throw 'Rust select failed' }
    $null = Wait-For { $s = Rust-State $rust; $s.selected -eq $jid -and $s.messages_visible -ge 5 } 'selected chat and rendered bubbles'
    $z.app.Refresh(); $rust.Refresh()
    $null = Zig-Control $z @{cmd='select'; id=$jid}
    $null = Zig-Control $z @{cmd='theme'; theme='amoled'}
    $null = Wait-For { $m = Zig-Control $z @{cmd='messages';limit=80}; @($m).Count -gt 0 } 'Zig messages'
    $sendText = 'Synthetic benchmark send from Rust'
    $sent = Pipe "WazigRust-$($rust.Id)" "send $sendText"
    if (-not $sent.ok) { throw 'Control send was rejected' }
    $null = Wait-For { $s = Rust-State $rust; if ($s.last_send_status -eq 'failed') { throw $s.status }; $s.last_send_status -eq 'sent' } 'backend-confirmed Rust send'
    $backend = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
    if (@($backend.sends | Where-Object { $_.text -eq $sendText -and $_.chat -eq $jid }).Count -ne 1) { throw 'Server did not receive exactly one valid send' }
    $null = Wait-For { (Rust-State $rust).messages_loaded -ge 6 } 'sent message history reload'
    $rustLoad = Memory $rust; $zigLoad = Memory $zig
    # Actual WM_KEYDOWN, not synthetic timing values: each sample waits for EndDraw acknowledgement.
    $rust.Refresh()
    $before = @((Rust-State $rust).key_to_paint_ms).Count
    for ($i = 0; $i -lt 50; $i++) {
        if (-not [NativeBench]::PostMessage($rust.MainWindowHandle, 0x100, [UIntPtr]0x28, [IntPtr]::Zero)) { throw 'Cannot post Down key' }
        $target = $before + $i + 1
        $null = Wait-For { @((Rust-State $rust).key_to_paint_ms).Count -ge $target } 'Down key painted' 10
    }
    $latencies = @((Rust-State $rust).key_to_paint_ms | Select-Object -Last 50)
    $null = Pipe "WazigRust-$($rust.Id)" "select $jid"
    $null = Wait-For { (Rust-State $rust).messages_visible -ge 6 } 'screenshot fake chat'
    $null = [NativeBench]::SetForegroundWindow($rust.MainWindowHandle)
    $windowRect = [NativeBench+Rect]::new()
    $null = [NativeBench]::GetWindowRect($rust.MainWindowHandle, [ref]$windowRect)
    $bitmap = [Drawing.Bitmap]::new($windowRect.Right-$windowRect.Left, $windowRect.Bottom-$windowRect.Top)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try { if (-not [NativeBench]::PrintWindow($rust.MainWindowHandle, $dc, 2)) { throw 'Screenshot capture failed' } }
    finally { $graphics.ReleaseHdc($dc) }
    $bitmap.Save((Join-Path $evidence 'rust-FAKE-data.png'), [Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose(); $bitmap.Dispose()
    # No sampling/foreground/key input during the two-minute idle period.
    Start-Sleep -Seconds 120
    $rustIdle = Memory $rust; $zigIdle = Memory $zig
    $rustBytes = (Get-Item $rustExe).Length; $zigBytes = (Get-Item $zigExe).Length
    $numbers = [ordered]@{
        rust_startup_ms = $rustStartup; rust_observed_startup_ms = $rustObserved; zig_observed_startup_ms = $zigStartup
        rust_key_to_paint_ms = $latencies; rust_load = $rustLoad; zig_load = $zigLoad
        rust_idle = $rustIdle; zig_idle = $zigIdle; rust_exe_bytes = $rustBytes; zig_exe_bytes = $zigBytes
        zig_version = $latest.tagName; rust_commit = $env:GITHUB_SHA
        state = (Rust-State $rust); backend_send_count = $backend.sends.Count
    }
    $numbers | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'measurements.json')
    $report = @"
# Wazig native WhatsApp benchmark

200 synthetic chats; same loopback WhatsApp hub and fake image/reply fixture. Windows hosted runner, x64. Rust commit $env:GITHUB_SHA; Zig $($latest.tagName).

| Metric | Rust | Zig $($latest.tagName) |
|---|---:|---:|
| Startup to loaded/painted chat list, observed median (10 runs), ms | $([Math]::Round((Median $rustObserved), 2)) | $([Math]::Round((Median $zigStartup), 2)) |
| Startup to first chat EndDraw, instrumented median (10 runs), ms | $([Math]::Round((Median $rustStartup), 2)) | not instrumented |
| Down WM_KEYDOWN to EndDraw, median (50), ms | $([Math]::Round((Median $latencies), 2)) | not instrumented |
| Working Set after load, MiB | $($rustLoad.working_set) | $($zigLoad.working_set) |
| Private Bytes after load, MiB | $($rustLoad.private_bytes) | $($zigLoad.private_bytes) |
| Working Set after 2 minutes idle, MiB | $($rustIdle.working_set) | $($zigIdle.working_set) |
| Private Bytes after 2 minutes idle, MiB | $($rustIdle.private_bytes) | $($zigIdle.private_bytes) |
| Executable size, bytes | $rustBytes | $zigBytes |

PASS: Rust launched, painted 200 chats, selected a chat, rendered messages, and sent a message confirmed independently by the fake server. Screenshot contains FAKE data only.

Method: ten new processes, fresh isolated profile/cache each time; OS disk cache is not purged (process-cold, not machine-cold). Observed startup uses the same 20 ms control polling, including pipe connection overhead. Zig readiness is chat_count=200 plus a nonzero list-paint counter, a proxy rather than exact first-paint instrumentation. Rust's extra instrumented row measures process creation to the first successful chat-list EndDraw. Latency ends after EndDraw (CPU submission), not physical display scanout. Retained final processes share the same runner and backend; Zig uses its published --verify-fixture isolation exactly as its harness does. Rust opt-level=z; no opt-level=3 comparison has been measured. Memory is process WorkingSet64/PrivateMemorySize64, not Task Manager's active private working set.
"@
    $report | Set-Content (Join-Path $evidence 'bench.md')
    $report | Out-File $env:GITHUB_STEP_SUMMARY -Append -Encoding utf8
    Write-Output $report
} finally {
    Stop-App $script:rust; Stop-App $script:zig; Stop-App $server
}
