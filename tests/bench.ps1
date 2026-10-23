$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'Benchmarks run only on GitHub Actions' }
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class NativeBench {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct StartupInfo {
        public int cb; public string reserved, desktop, title;
        public int x, y, xSize, ySize, xCountChars, yCountChars, fillAttribute, flags;
        public short showWindow, reserved2Size;
        public IntPtr reserved2, standardInput, standardOutput, standardError;
    }
    [StructLayout(LayoutKind.Sequential)] public struct ProcessInformation {
        public IntPtr Process, Thread; public int ProcessId, ThreadId;
    }
    [DllImport("kernel32.dll", EntryPoint="CreateProcessW", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool CreateProcess(string app, StringBuilder command, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr environment, string dir, ref StartupInfo startup, out ProcessInformation process);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static int Launch(string app, string args, string[] environment, int show) {
        Array.Sort(environment, StringComparer.OrdinalIgnoreCase);
        IntPtr block = Marshal.StringToHGlobalUni(string.Join("\0", environment) + "\0\0");
        try {
            var startup = new StartupInfo(); startup.cb = Marshal.SizeOf(typeof(StartupInfo));
            if (show >= 0) { startup.flags = 1; startup.showWindow = (short)show; }
            ProcessInformation process;
            if (!CreateProcess(app, new StringBuilder("\"" + app + "\" " + args), IntPtr.Zero, IntPtr.Zero, false, 0x400, block, null, ref startup, out process))
                throw new InvalidOperationException("CreateProcess: " + Marshal.GetLastWin32Error());
            CloseHandle(process.Process); CloseHandle(process.Thread); return process.ProcessId;
        } finally { Marshal.FreeHGlobal(block); }
    }
    [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr window);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, UIntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int command);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int height, uint flags);
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
$script:localApp = $null

function Wait-For([scriptblock]$Check, [string]$What, [int]$Seconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $result = & $Check
        if ($result) { return $result }
        Start-Sleep -Milliseconds 20
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($script:rust) { try { Rust-State $script:rust | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'timeout-rust.json') } catch {} }
    if ($script:zig -and $script:zigSession) { try { Zig-Control $script:zigSession @{cmd='status'} | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'timeout-zig.json') } catch {} }
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
function Start-App([string]$Exe, [string]$Kind, [string]$Endpoint = '', [int]$Show = -1) {
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
    $info.Environment['WAZIG_HUB_URL'] = if ($Endpoint) { $Endpoint } else { $script:hub }
    $info.Environment['WAZIG_FIXTURE_URL'] = $script:hub
    $info.Environment['WAZIG_HUB_TOKEN'] = 'fixture-key-not-a-secret'
    if ($Kind -eq 'local') {
        $info.Environment['WAZIG_HUB_URL'] = ''
        $info.Environment['WAZIG_HUB_TOKEN'] = ''
        $cliDir = Join-Path $local 'Programs/wacli'
        New-Item $cliDir -ItemType Directory -Force | Out-Null
        Copy-Item 'target/release/fake-wacli.exe' (Join-Path $cliDir 'wacli.exe')
    }
    foreach ($key in @('DEEPGRAM_API_KEY','OPENROUTER_API_KEY','WA_BRIDGE_KEY','GH_TOKEN','GITHUB_TOKEN')) { $null = $info.Environment.Remove($key) }
    if ($Kind -eq 'zig') {
        $info.ArgumentList.Add('--verify-fixture')
        $info.Environment['WAZIG_VERIFY_ROOT'] = $dir
        $info.Environment['WAZIG_VERIFY_ID'] = $id
    }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $environment = [string[]]@($info.Environment.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" })
    $processId = [NativeBench]::Launch($Exe, ($info.ArgumentList -join ' '), $environment, $Show)
    $app = [Diagnostics.Process]::GetProcessById($processId)
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
function Key-Latencies($App) {
    $App.Refresh()
    $before = @((Rust-State $App).key_to_paint_ms).Count
    for ($keyIndex = 0; $keyIndex -lt 50; $keyIndex++) {
        if (-not [NativeBench]::PostMessage($App.MainWindowHandle, 0x100, [UIntPtr]0x28, [IntPtr]::Zero)) { throw 'Cannot post Down key' }
        $target = $before + $keyIndex + 1
        $null = Wait-For { @((Rust-State $App).key_to_paint_ms).Count -ge $target } 'Down key painted' 10
    }
    return @((Rust-State $App).key_to_paint_ms | Select-Object -Last 50)
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
    $profile3Exe = (Resolve-Path 'target/release/wazig-rust-3.exe').Path
    $profile3Startup = @()
    for ($i = 0; $i -lt 10; $i++) {
        $candidate = Start-App $profile3Exe 'rust'
        $script:rust = $candidate.app
        $candidateState = Wait-For {
            if ($candidate.app.HasExited) { throw 'opt-level=3 candidate exited' }
            try { $s = Rust-State $candidate.app; if ($s.chats -eq 200 -and $null -ne $s.first_chat_painted_unix_ms) { $s } } catch [TimeoutException] {}
        } 'opt-level=3 first chat paint'
        $epoch = ([DateTimeOffset]$candidate.app.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
        $profile3Startup += ([double]$candidateState.first_chat_painted_unix_ms - $epoch)
        if ($i -eq 9) { $profile3Keys = @(Key-Latencies $candidate.app) }
        Stop-App $candidate.app
        $script:rust = $null
    }
    $rustStartup = @(); $rustObserved = @(); $zigStartup = @()
    # Each process gets an empty private profile: includes font extraction and media/cache setup.
    for ($i = 0; $i -lt 10; $i++) {
        $r = Start-App $rustExe 'rust' '' (@(-1,1,2)[$i % 3])
        $script:rust = $r.app
        $state = Wait-For {
            if ($r.app.HasExited) { throw "Rust exited: $($r.app.ExitCode)" }
            try { $s = Rust-State $r.app; if ($s.chats -eq 200 -and $null -ne $s.first_chat_painted_unix_ms) { return $s } } catch [TimeoutException] {}
        } 'Rust first painted 200-chat list'
        $r.app.Refresh()
        if (-not [NativeBench]::IsZoomed($r.app.MainWindowHandle) -or -not $state.maximized) { throw "Rust not maximized for startup override $i" }
        $rustObserved += $r.clock.Elapsed.TotalMilliseconds
        $startEpoch = ([DateTimeOffset]$r.app.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
        $rustStartup += ([double]$state.first_chat_painted_unix_ms - $startEpoch)
        $null = Wait-For { $s = Rust-State $r.app; if ($s.messages_visible -gt 0) { $s } } 'Rust opened initial chat'
        if ($i -lt 9) { Stop-App $r.app; $script:rust = $null }
        $z = Start-App $zigExe 'zig'
        $script:zig = $z.app
        $null = Wait-For {
            if ($z.app.HasExited) { throw "Zig exited: $($z.app.ExitCode)" }
            try { $s = Zig-Control $z @{cmd='status'}; if ($s.chat_count -ge 200 -and $s.paints.list -gt 0) { return $s } } catch [TimeoutException] {}
        } 'Zig 200 chats and list paint'
        $zigStartup += $z.clock.Elapsed.TotalMilliseconds
        if ($i -lt 9) { Stop-App $z.app; $script:zig = $null }
    }
    $expected = Invoke-RestMethod "$hub/fixture/expected" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
    if ($expected.Count -ne 200) { throw "Invalid fixture oracle count $($expected.Count)" }
    $state = Rust-State $rust
    $state | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'round2-chat-state.json')
    $expected | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'round2-expected.json')
    if ($state.chat_rows.Count -ne 200) { throw 'Rust junk filtering failed' }
    for ($i = 0; $i -lt 200; $i++) {
        if ($state.chat_rows[$i].jid -cne $expected[$i].jid -or $state.chat_rows[$i].name -cne $expected[$i].name) { throw "Wrong Rust order/name at ${i}: got $($state.chat_rows[$i].jid)/$($state.chat_rows[$i].name), expected $($expected[$i].jid)/$($expected[$i].name)" }
    }
    $null = Zig-Control $z @{cmd='view';messenger='whatsapp'}
    $zc = Zig-Control $z @{cmd='chats';limit=250}
    # The hub normally filters service rows; inject them here to require Rust to reject them too.
    $zigRows = @($zc.chats | Where-Object { $_.id -notmatch '^(personal:)?(0@s\.whatsapp\.net|status@broadcast)$' })
    if ($zigRows.Count -ne 200) { throw "Unexpected Zig comparison count $($zigRows.Count)" }
    for ($i = 0; $i -lt 200; $i++) {
        if ($zigRows[$i].id -cne $expected[$i].jid -or $zigRows[$i].name -cne $expected[$i].name) { throw "Rust/oracle disagrees with Zig at $i" }
    }
    # Both retained final processes are loaded against the very same server/fixtures.
    $jid = 'personal:15550000001@s.whatsapp.net'
    $selected = Pipe "WazigRust-$($rust.Id)" "select $jid"
    if (-not $selected.ok) { throw 'Rust select failed' }
    $null = Wait-For { $s = Rust-State $rust; $s.selected -eq $jid -and $s.messages_loaded -ge 5 -and $s.messages_visible -gt 0 } 'selected chat and rendered bubbles'
    $z.app.Refresh(); $rust.Refresh()
    $null = Zig-Control $z @{cmd='select'; id=$jid}
    $null = Zig-Control $z @{cmd='theme'; theme='amoled'}
    $null = Wait-For { $m = Zig-Control $z @{cmd='messages';limit=80}; @($m.messages).Count -ge 5 } 'Zig messages'
    $null = Wait-For { (Rust-State $rust).painted_avatars -gt 0 } 'round photo avatars'
    $sendText = 'Synthetic benchmark send from Rust'
    $sent = Pipe "WazigRust-$($rust.Id)" "send $sendText"
    if (-not $sent.ok) { throw 'Control send was rejected' }
    $null = Wait-For { $s = Rust-State $rust; if ($s.last_send_status -eq 'failed') { throw $s.status }; $s.last_send_status -eq 'sent' } 'backend-confirmed Rust send'
    $backend = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
    if (@($backend.sends | Where-Object { $_.text -eq $sendText -and $_.chat -eq $jid }).Count -ne 1) { throw 'Server did not receive exactly one valid send' }
    $null = Wait-For { (Rust-State $rust).messages_loaded -ge 6 } 'sent message history reload'
    # Drive real keyboard input too, including the command popup and C focus shortcut.
    $rust.Refresh()
    if (-not [NativeBench]::SetForegroundWindow($rust.MainWindowHandle)) { throw 'Cannot focus Rust keyboard test' }
    [Windows.Forms.SendKeys]::SendWait('^k')
    $null = Wait-For { (Rust-State $rust).focus -eq 'commands' } 'Ctrl+K command popup'
    [Windows.Forms.SendKeys]::SendWait('{ESC}c')
    $null = Wait-For { $s = Rust-State $rust; $s.focus -eq 'composer' -and $s.composer_characters -eq 0 } 'C shortcut without inserting c'
    [Windows.Forms.SendKeys]::SendWait('Synthetic keyboard send{ENTER}')
    $null = Wait-For {
        $b = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
        @($b.sends | Where-Object { $_.text -eq 'Synthetic keyboard send' }).Count -eq 1
    } 'Enter key sends composer text'
    $null = Wait-For { (Rust-State $rust).last_send_status -eq 'sent' } 'keyboard send acknowledgement'
    [Windows.Forms.SendKeys]::SendWait('{ESC}')
    $null = Wait-For { (Rust-State $rust).focus -eq 'list' } 'Esc back to list'
    $rustLoad = Memory $rust; $zigLoad = Memory $zig
    # Actual WM_KEYDOWN, not synthetic timing values: each sample waits for EndDraw acknowledgement.
    $latencies = @(Key-Latencies $rust)
    $null = Pipe "WazigRust-$($rust.Id)" "select $jid"
    $null = Wait-For { (Rust-State $rust).messages_loaded -ge 6 -and (Rust-State $rust).messages_visible -gt 0 -and (Rust-State $rust).sidebar_scroll -eq 0 -and (Rust-State $rust).images_cached -gt 0 } 'screenshot fake chat'
    $null = [NativeBench]::SetForegroundWindow($rust.MainWindowHandle)
    $paintBeforeResize = (Rust-State $rust).paints
    $null = [NativeBench]::ShowWindow($rust.MainWindowHandle, 1)
    $null = [NativeBench]::SetWindowPos($rust.MainWindowHandle, [IntPtr]::Zero, 0, 0, 1920, 1032, 0x0040)
    $null = Wait-For { (Rust-State $rust).paints -gt $paintBeforeResize } '1920x1032 screenshot repaint'
    $windowRect = [NativeBench+Rect]::new()
    $null = [NativeBench]::GetWindowRect($rust.MainWindowHandle, [ref]$windowRect)
    if ($windowRect.Right-$windowRect.Left -ne 1920 -or $windowRect.Bottom-$windowRect.Top -ne 1032) { throw 'Screenshot window did not reach 1920x1032' }
    $bitmap = [Drawing.Bitmap]::new($windowRect.Right-$windowRect.Left, $windowRect.Bottom-$windowRect.Top)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    $dc = $graphics.GetHdc()
    try { if (-not [NativeBench]::PrintWindow($rust.MainWindowHandle, $dc, 2)) { throw 'Screenshot capture failed' } }
    finally { $graphics.ReleaseHdc($dc) }
    $bitmap.Save((Join-Path $evidence 'rust-FAKE-data.png'), [Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose(); $bitmap.Dispose()
    $idlePaint = (Rust-State $rust).paints
    $null = [NativeBench]::ShowWindow($rust.MainWindowHandle, 3)
    $null = Wait-For { (Rust-State $rust).paints -gt $idlePaint } 'restored maximized repaint'
    # No sampling/foreground/key input during the two-minute idle period.
    Start-Sleep -Seconds 120
    $rustIdle = Memory $rust; $zigIdle = Memory $zig
    # Verify the normal-install backend too, including default executable/store paths and exact argv.
    $localSession = Start-App $rustExe 'local'
    $script:localApp = $localSession.app
    $localState = Wait-For { $s = Rust-State $localApp; if ($s.chats -eq 4 -and $s.messages_visible -gt 0) { $s } } 'local wacli default-store history'
    if ($localSession.clock.Elapsed.TotalSeconds -ge 8) { throw 'Slow media blocked local history' }
    $localNames = @('Fixture Local Store','Fixture Official Group','Fixture Push Name','Fixture System Name')
    if (($localState.chat_rows.name -join '|') -cne ($localNames -join '|')) { throw 'Contact/group resolution or pinned/timestamp/name order failed' }
    if (($localState.message_ids -join '|') -cne 'BASE|SLOW_IMAGE') { throw 'Wrapped, paginated wacli history not parsed in chronological order' }
    $null = Wait-For { Test-Path (Join-Path $localSession.root 'profile/.wacli/slow-mark-read') } 'wacli write lock wait started' 3
    $null = Pipe "WazigRust-$($localApp.Id)" 'select 15550000997@s.whatsapp.net'
    $null = Wait-For { $s = Rust-State $localApp; $s.selected -eq '15550000997@s.whatsapp.net' -and $s.messages_visible -gt 0 } 'switch during blocked media and mark-read' 3
    $null = Pipe "WazigRust-$($localApp.Id)" 'select 15550000999@s.whatsapp.net'
    $null = Wait-For { (Rust-State $localApp).messages_visible -gt 0 } 'return to local chat' 3
    $fontLicense = Join-Path $localSession.root 'local/WazigRust/fonts/IBM-Plex-LICENSE.txt'
    if ((Get-FileHash $fontLicense).Hash -ne (Get-FileHash 'assets/IBM-Plex-LICENSE.txt').Hash) { throw 'Portable font licence missing or incorrect' }
    if ($localState.selected -ne '15550000999@s.whatsapp.net') { throw 'Wrong local-store chat' }
    $null = Pipe "WazigRust-$($localApp.Id)" 'send Synthetic local store send'
    $localState = Wait-For { $s = Rust-State $localApp; if ($s.last_send_status -eq 'failed') { throw $s.status }; if ($s.last_send_status -eq 'sent') { $s } } 'local wacli send'
    $store = Join-Path $localSession.root 'profile/.wacli'
    if ((Get-Content (Join-Path $store 'sent.txt') -Raw) -ne 'Synthetic local store send') { throw 'Send did not reach default wacli store' }
    Copy-Item (Join-Path $store 'argv.jsonl') (Join-Path $evidence 'local-wacli-argv.jsonl')
    $localState | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $evidence 'local-wacli-state.json')
    $null = Wait-For { (Rust-State $localApp).painted_avatars -gt 0 } 'wacli profile URL and round mask' 45
    Stop-App $localApp; $script:localApp = $null
    Write-Output 'ROUND2 PASS: maximized order names messages'
    Write-Output 'ROUND2 PASS: round photo avatars'
    Stop-App $rust; Stop-App $zig
    $script:rust = $null; $script:zig = $null
    # Both large-image processes get fresh profiles and identical 50 x 4000x3000 JPEGs.
    $largeRustHub = 'http://127.0.0.1:' + (Get-Content (Join-Path $root 'large-rust-port.txt') -Raw).Trim()
    $largeZigHub = 'http://127.0.0.1:' + (Get-Content (Join-Path $root 'large-zig-port.txt') -Raw).Trim()
    $largeR = Start-App $rustExe 'rust' $largeRustHub
    $script:rust = $largeR.app
    $largeZ = Start-App $zigExe 'zig' $largeZigHub
    $script:zig = $largeZ.app
    $script:zigSession = $largeZ
    $null = Wait-For { (Rust-State $script:rust).chats -eq 5 } 'large Rust chat list'
    $null = Wait-For { (Zig-Control $largeZ @{cmd='status'}).chat_count -eq 5 } 'large Zig chat list'
    $null = Zig-Control $largeZ @{cmd='theme';theme='amoled'}
    $seen = [Collections.Generic.HashSet[string]]::new()
    $largeTrace = [Collections.Generic.List[object]]::new()
    $largeExpected = Invoke-RestMethod "$largeRustHub/fixture/expected" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
    if ($largeExpected.Count -ne 5) { throw 'Invalid large-image chat oracle' }
    foreach ($chat in $largeExpected) {
        $null = Pipe "WazigRust-$($script:rust.Id)" "select $($chat.jid)"
        $null = Zig-Control $largeZ @{cmd='select';id=$chat.jid}
        $null = Wait-For { $s = Rust-State $script:rust; $s.selected -eq $chat.jid -and $s.messages_loaded -eq 10 } 'large Rust history'
        $null = Wait-For { @((Zig-Control $largeZ @{cmd='messages';limit=80}).messages).Count -eq 10 } 'large Zig history'
        # Before scrolling, the other photos in this chat must not have been eagerly fetched.
        $requests = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
        $forChat = @($requests.media_requests.rust | Where-Object { $_ -like "*/$([uri]::EscapeDataString($chat.jid))/*" })
        if ($forChat.Count -ge 10) { throw 'Rust eagerly downloaded off-screen photos' }
        $script:zig.Refresh()
        $wr = [NativeBench+Rect]::new()
        $null = [NativeBench]::GetWindowRect($script:zig.MainWindowHandle, [ref]$wr)
        $px = [int]($wr.Left + ($wr.Right-$wr.Left)*3/4)
        $py = [int]($wr.Top + ($wr.Bottom-$wr.Top)/2)
        $point = [IntPtr](($py -shl 16) -bor ($px -band 0xffff))
        for ($step = 0; $step -lt 35; $step++) {
            $s = Wait-For {
                $v = Rust-State $script:rust
                if ($v.painted_images.Count -gt 0 -and $v.painted_images.Count -eq $v.visible_images.Count) { $v }
            } 'visible large JPEG decoded'
            foreach ($id in $s.painted_images) { $null = $seen.Add($id) }
            if ($s.bitmap_bytes -gt 4MB -or $s.images_cached -gt 32) { throw 'Decoded bitmap LRU exceeded its budget' }
            $largeTrace.Add(@{ chat=$chat.jid; step=$step; state=$s; rust=(Memory $script:rust); zig=(Memory $script:zig) })
            $paint = $s.paints
            $null = Pipe "WazigRust-$($script:rust.Id)" 'scroll 140'
            $null = Wait-For { (Rust-State $script:rust).paints -gt $paint } 'Rust image scroll paint'
            $zPaint = (Zig-Control $largeZ @{cmd='status'}).paints.canvas
            $null = [NativeBench]::PostMessage($script:zig.MainWindowHandle, 0x20A, [UIntPtr](280 -shl 16), $point)
            $null = Wait-For { (Zig-Control $largeZ @{cmd='status'}).paints.canvas -gt $zPaint } 'Zig image scroll paint'
        }
        # Zig downloads serially and refreshes history after each result. Do not leave a
        # chat before its queue drains, or the next selection can cancel that work.
        $null = Wait-For {
            $v = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
            $v | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $evidence 'large-requests.json')
            $pattern = "*/$([uri]::EscapeDataString($chat.jid))/*"
            $rc = @($v.media_requests.rust | Where-Object { $_ -like $pattern }).Count
            $zc = @($v.media_requests.zig | Where-Object { $_ -like $pattern }).Count
            if ($rc -eq 10 -and $zc -eq 10) { $true }
        } "both apps downloaded all ten JPEGs for $($chat.jid)" 60
        # Revisit Zig's whole history after the serial downloads, so comparison includes decoding.
        for ($step = 0; $step -lt 35; $step++) {
            $zPaint = (Zig-Control $largeZ @{cmd='status'}).paints.canvas
            $null = [NativeBench]::PostMessage($script:zig.MainWindowHandle, 0x20A, [UIntPtr]([uint64]65256 -shl 16), $point)
            $null = Wait-For { (Zig-Control $largeZ @{cmd='status'}).paints.canvas -gt $zPaint } 'Zig cached JPEG scroll paint'
        }
        $null = Pipe "WazigRust-$($script:rust.Id)" 'scroll -100000'
        $null = Wait-For { $v = Rust-State $script:rust; $v.painted_images.Count -gt 0 -and $v.painted_images.Count -eq $v.visible_images.Count } 'Rust final large JPEG viewport'
    }
    if ($seen.Count -ne 50) { throw "Only $($seen.Count)/50 large photos actually painted" }
    $requests = Wait-For {
        $v = Invoke-RestMethod "$hub/fixture/state" -Headers @{Authorization='Bearer fixture-key-not-a-secret'}
        if (@($v.media_requests.rust).Count -eq 50 -and @($v.media_requests.zig).Count -eq 50) { $v }
    } 'both apps downloaded all 50 large JPEGs' 90
    $largeTrace | ConvertTo-Json -Depth 12 | Set-Content (Join-Path $evidence 'large-image-trace.json')
    $largeSamples = @()
    # Same quiet one-minute window; compare unrounded median private bytes, no working-set trimming.
    for ($i = 0; $i -lt 12; $i++) {
        Start-Sleep -Seconds 5
        $script:rust.Refresh(); $script:zig.Refresh()
        $largeSamples += @{rust=(Memory $script:rust);zig=(Memory $script:zig);rust_private=$script:rust.PrivateMemorySize64;zig_private=$script:zig.PrivateMemorySize64}
    }
    $largeRust = @{working_set=(Median $largeSamples.rust.working_set);private_bytes=(Median $largeSamples.rust.private_bytes)}
    $largeZig = @{working_set=(Median $largeSamples.zig.working_set);private_bytes=(Median $largeSamples.zig.private_bytes)}
    $largeProof = @{rust=$largeRust;zig=$largeZig;samples=$largeSamples;painted=$seen.Count;requests=$requests;rust_commit=$env:GITHUB_SHA}
    $largeProof | ConvertTo-Json -Depth 12 | Set-Content (Join-Path $evidence 'large-image-memory.json')
    Write-Output "Large JPEG memory MiB: Rust WS=$($largeRust.working_set) private=$($largeRust.private_bytes); Zig WS=$($largeZig.working_set) private=$($largeZig.private_bytes)"
    if ((Median $largeSamples.rust_private) -gt (Median $largeSamples.zig_private)) { throw 'Rust large-image private memory exceeds Zig' }
    Write-Output 'ROUND2 PASS: large-image memory rust<=zig'
    $rustBytes = (Get-Item $rustExe).Length; $zigBytes = (Get-Item $zigExe).Length
    $numbers = [ordered]@{
        rust_startup_ms = $rustStartup; rust_observed_startup_ms = $rustObserved; zig_observed_startup_ms = $zigStartup
        rust_key_to_paint_ms = $latencies; rust_load = $rustLoad; zig_load = $zigLoad
        large_rust = $largeRust; large_zig = $largeZig
        rust_idle = $rustIdle; zig_idle = $zigIdle; rust_exe_bytes = $rustBytes; zig_exe_bytes = $zigBytes
        profile3_startup_ms = $profile3Startup; profile3_keys_ms = $profile3Keys; profile3_exe_bytes = (Get-Item $profile3Exe).Length
        zig_version = $latest.tagName; rust_commit = $env:GITHUB_SHA
        state = (Rust-State $script:rust); backend_send_count = $backend.sends.Count
    }
    $numbers | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $evidence 'measurements.json')
    $report = @"
# Wazig native WhatsApp benchmark

200 synthetic chats (plus injected junk), mixed activity timestamps, contact/push/group names; same loopback WhatsApp hub and fake image/reply fixture. Windows hosted runner, x64. Rust commit $env:GITHUB_SHA; Zig $($latest.tagName).

| Metric | Rust | Zig $($latest.tagName) |
|---|---:|---:|
| Startup to loaded/painted chat list, observed median (10 runs), ms | $([Math]::Round((Median $rustObserved), 2)) | $([Math]::Round((Median $zigStartup), 2)) |
| Startup to first chat EndDraw, instrumented median (10 runs), ms | $([Math]::Round((Median $rustStartup), 2)) | not instrumented |
| Down WM_KEYDOWN to EndDraw, median (50), ms | $([Math]::Round((Median $latencies), 2)) | not instrumented |
| Working Set after load, MiB | $($rustLoad.working_set) | $($zigLoad.working_set) |
| Private Bytes after load, MiB | $($rustLoad.private_bytes) | $($zigLoad.private_bytes) |
| Working Set after 2 minutes idle, MiB | $($rustIdle.working_set) | $($zigIdle.working_set) |
| Private Bytes after 2 minutes idle, MiB | $($rustIdle.private_bytes) | $($zigIdle.private_bytes) |
| Working Set, 50 large JPEGs + 1 minute idle, MiB | $($largeRust.working_set) | $($largeZig.working_set) |
| Private Bytes, 50 large JPEGs + 1 minute idle, MiB | $($largeRust.private_bytes) | $($largeZig.private_bytes) |
| Executable size, bytes | $rustBytes | $zigBytes |

PASS: Rust launched, painted 200 chats, selected a chat, rendered messages, and sent a message confirmed independently by the fake server. The normal-install wacli backend also loaded history and sent successfully through the default executable and store paths with exact argv assertions. Screenshot contains FAKE data only.

Method: ten new processes, fresh isolated profile/cache each time; OS disk cache is not purged (process-cold, not machine-cold). Observed startup uses the same 20 ms control polling, including pipe connection overhead. Zig readiness is chat_count=200 plus a nonzero list-paint counter, a proxy rather than exact first-paint instrumentation. Rust's extra instrumented row measures process creation to the first successful chat-list EndDraw. Latency ends after EndDraw (CPU submission), not physical display scanout. Retained final processes share the same runner and backend; Zig uses its published --verify-fixture isolation exactly as its harness does. Release profile selection: opt-level=z executable $rustBytes bytes versus opt-level=3 $((Get-Item $profile3Exe).Length) bytes; opt-level=3 instrumented startup median $([Math]::Round((Median $profile3Startup),2)) ms and key-to-EndDraw median $([Math]::Round((Median $profile3Keys),2)) ms. The default z favours executable footprint; both candidates were built and measured on this runner. Memory is process WorkingSet64/PrivateMemorySize64, not Task Manager's active private working set.
"@
    $report | Set-Content (Join-Path $evidence 'bench.md')
    $report | Out-File $env:GITHUB_STEP_SUMMARY -Append -Encoding utf8
    Write-Output $report
} finally {
    if ($largeTrace) { $largeTrace | ConvertTo-Json -Depth 12 | Set-Content (Join-Path $evidence 'large-image-trace.json') }
    Stop-App $script:rust; Stop-App $script:zig; Stop-App $script:localApp; Stop-App $server
}
