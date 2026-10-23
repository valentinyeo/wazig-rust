# Wazig Rust

A small native Windows x64 WhatsApp client. Rust stable, Win32 window/input,
Direct2D bubbles, DirectWrite colour emoji and private IBM Plex Sans, WIC images.
No browser, webview, GUI framework, or embedded WhatsApp session.

## Run

Download `WazigRust.exe` from [Releases](https://github.com/valentinyeo/wazig-rust/releases).
It opens maximized in the AMOLED theme.

With no hub configured, it runs `%LOCALAPPDATA%\Programs\wacli\wacli.exe`, using
wacli's existing `%USERPROFILE%\.wacli` store. Log in/sync using your existing
wacli installation first. List/history requests use `--json --read-only`; sends
use `--json --lock-wait 0s send text --to <jid> --message <text>`, matching Zig.
`WAZIG_RUST_WACLI` can override the executable path, not the store.

When `WAZIG_HUB_URL` is set, the app instead uses the same authenticated
`/v1/wazig/` API as Zig, filtering out non-WhatsApp chats.
Set `WAZIG_HUB_TOKEN` as well. Saved HKCU Environment values take priority over
inherited process values, like Zig. HTTPS is required except on loopback.
Tokens are never written to disk. A token saved only in Zig's encrypted settings
is **not** imported: explicitly provide the environment token for Rust.

Only `%LOCALAPPDATA%\WazigRust` is used for Rust fonts, downloaded hub images and
local unread overrides. Rust never writes Zig's settings. Archive/read/send
operations intentionally update the shared WhatsApp backend.

## Controls

- Up/Down or J/K: select chats; mouse wheel: scroll the hovered pane.
- Enter or C: compose; Enter: send; Shift+Enter: new line; Ctrl+V: paste.
- Ctrl+F or `/`: search names/JIDs; Esc: back to list and clear search.
- Ctrl+K: filterable commands; Up/Down + Enter or click to execute; F1: help.
- E: archive (or unarchive in the archive view); Ctrl+A from list: archive view.
- U: toggle local unread override; R: refresh; Q: quit (list focus).

Network and child processes run on a background worker. Chat/history polling
is every five seconds; selected-chat responses are identity checked. Images
use an eight-entry device-bitmap LRU capped at 960 pixels on the long edge.
Text layouts use a 256-entry cache.

This is an initial comparison client, not a full replacement: no QR login UI,
attachment sending, editing/selection in the custom composer, on-screen reaction
picker, old-history pagination, or durable offline outbox. The plus and emoji
controls are visual placeholders. Existing replies/reactions/images are rendered.
Hub sends retry once using the same client ID after an ambiguous failure.
A failed send keeps the draft. The legacy account-scoped WA bridge API is not
implemented; unified hub and local wacli are supported.

## CI control channel

Each instance serves the local-only named pipe `\\.\pipe\WazigRust-<pid>`.
Send one UTF-8 newline-terminated command, read one JSON line:

- `state`: chat count, selection, visible/loaded message counts, send status,
  focus, paint count, first-chat paint timing and key-to-EndDraw samples.
- `select <jid>`: select a known chat (marks read).
- `send <text>`: send to the selected chat; poll `last_send_status`.

Do not expose the pipe to untrusted local processes: send is a real backend action.

## Verification and measurement

**Do not build/test/run on the VPS.** The `ci` workflow runs release clippy,
tests and build on hosted Windows, then publishes a versioned executable.
The `bench` workflow launches both Rust and the latest published Zig executable
against 200 entirely fake WhatsApp chats. It independently verifies the server
received the send, measures ten process startups, fifty Down-to-EndDraw samples,
Working Set/Private Bytes at load and after two idle minutes, and executable size.
A fake-data screenshot, raw measurements and `bench.md` are Actions artifacts.
See the job summary for numbers and measurement caveats (including Zig's startup
paint-counter proxy and process-cold, not machine-cold, startup).

Release uses opt-level=z, LTO, one codegen unit, abort panics and stripped symbols.
The benchmark also builds opt-level=3 and measures size, ten startups and fifty
key-to-EndDraw samples to document the size/speed trade-off, rather than claiming
that z is universally fastest. GitHub hosted GPU/memory measurements are not
an exact prediction of Task Manager on a laptop.

IBM Plex Sans is distributed under its bundled SIL Open Font License.
