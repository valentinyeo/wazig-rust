use crate::model::{self, Chat, Message};
use serde_json::{json, Value};
use std::os::windows::process::CommandExt;
use std::{
    collections::HashMap,
    env,
    hash::{Hash, Hasher},
    path::PathBuf,
    process::Command,
    sync::mpsc,
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{Foundation::*, Networking::WinHttp::*, System::Registry::*},
};

pub enum Job {
    Chats(bool),
    Messages(String),
    Send(String, String),
    Archive(String, bool),
    MarkRead(String),
    Asset(String, String),
}
pub enum Event {
    Chats(bool, Result<Vec<Chat>, String>),
    Messages(String, Result<Vec<Message>, String>),
    Sent(String, String, Result<(), String>),
    Changed(Result<(), String>),
    Asset(String, String, Result<String, String>),
}

pub fn settings_dir() -> PathBuf {
    PathBuf::from(env::var_os("LOCALAPPDATA").unwrap_or_default()).join("WazigRust")
}

fn environment(name: &str) -> String {
    // Zig gives saved user environment priority over stale Explorer inheritance.
    unsafe {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut bytes = 0u32;
        if RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            None,
            Some(&mut bytes),
        )
        .is_ok()
            && bytes > 2
            && bytes <= 65536
            && bytes.is_multiple_of(2)
        {
            let mut value = vec![0u16; bytes as usize / 2];
            if RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Environment"),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                None,
                Some(value.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
            .is_ok()
            {
                let value = String::from_utf16_lossy(&value);
                let value = value.trim_end_matches('\0').trim();
                if !value.is_empty() {
                    return value.into();
                }
            }
        }
    }
    env::var(name).unwrap_or_default()
}

fn decrypt_token(bytes: &mut [u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::*;
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_mut_ptr(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        if output.cbData == 0 || output.pbData.is_null() {
            let _ = LocalFree(HLOCAL(output.pbData.cast()));
            return None;
        }
        let result = String::from_utf8(
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec(),
        )
        .ok();
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        result
    }
}

fn stored_hub_token() -> Option<String> {
    // Read Zig's existing DPAPI token, but never migrate or rewrite its settings.
    unsafe {
        let mut size = 0;
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Messages"),
            w!("HubToken"),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        if size == 0 || size > 65536 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Messages"),
            w!("HubToken"),
            RRF_RT_REG_BINARY,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        bytes.truncate(size as usize);
        decrypt_token(&mut bytes)
    }
}

struct HttpHandle(*mut std::ffi::c_void);
impl Drop for HttpHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}
impl HttpHandle {
    fn new(p: *mut std::ffi::c_void) -> Result<Self, String> {
        if p.is_null() {
            Err(windows::core::Error::from_win32().to_string())
        } else {
            Ok(Self(p))
        }
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Hub {
    base: String,
    token: String,
}
impl Hub {
    fn request(&self, path: &str, body: Option<&Value>) -> Result<Vec<u8>, String> {
        self.call(path, body).map_err(|e| e.to_string())
    }
    fn call(
        &self,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let (secure, base) = if let Some(s) = self.base.strip_prefix("https://") {
            (true, s)
        } else if let Some(s) = self.base.strip_prefix("http://") {
            (false, s)
        } else {
            return Err("Hub URL must start with https://".into());
        };
        let (authority, prefix) = base.split_once('/').unwrap_or((base, ""));
        let (host, port) = if let Some((h, p)) = authority.rsplit_once(':') {
            (h, p.parse::<u16>()?)
        } else {
            (authority, if secure { 443 } else { 80 })
        };
        if !secure && !["127.0.0.1", "localhost", "[::1]"].contains(&host) {
            return Err("Unencrypted hub connections are restricted to loopback".into());
        }
        if self.token.is_empty() || self.token.contains(['\r', '\n']) {
            return Err("Missing or invalid WAZIG_HUB_TOKEN".into());
        }
        let object = if prefix.is_empty() {
            path.to_owned()
        } else {
            format!("/{}{path}", prefix.trim_end_matches('/'))
        };
        unsafe {
            let session = HttpHandle::new(WinHttpOpen(
                w!("WazigRust/0.1"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ))?;
            WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000)?;
            let connection = HttpHandle::new(WinHttpConnect(
                session.0,
                PCWSTR(wide(host).as_ptr()),
                port,
                0,
            ))?;
            let request = HttpHandle::new(WinHttpOpenRequest(
                connection.0,
                if body.is_some() {
                    w!("POST")
                } else {
                    w!("GET")
                },
                PCWSTR(wide(&object).as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                if secure {
                    WINHTTP_FLAG_SECURE
                } else {
                    WINHTTP_OPEN_REQUEST_FLAGS(0)
                },
            ))?;
            let headers = wide(&format!(
                "Authorization: Bearer {}\r\nContent-Type: application/json\r\n",
                self.token
            ));
            let data = body.map(|v| v.to_string()).unwrap_or_default();
            WinHttpSendRequest(
                request.0,
                Some(&headers[..headers.len() - 1]),
                if data.is_empty() {
                    None
                } else {
                    Some(data.as_ptr().cast())
                },
                data.len() as u32,
                data.len() as u32,
                0,
            )?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;
            let mut status = 0u32;
            let mut size = 4u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut size,
                std::ptr::null_mut(),
            )?;
            if status != 200 {
                return Err(format!("Hub HTTP {status}").into());
            }
            let mut result = Vec::new();
            loop {
                let mut buf = [0u8; 16384];
                let mut got = 0;
                WinHttpReadData(
                    request.0,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut got,
                )?;
                if got == 0 {
                    break;
                }
                result.extend_from_slice(&buf[..got as usize]);
                if result.len() > 16 * 1024 * 1024 {
                    return Err("Hub response exceeds 16 MB".into());
                }
            }
            Ok(result)
        }
    }
}

struct Backend {
    hub: Option<Hub>,
    wacli: PathBuf,
    names: HashMap<String, String>,
    names_updated: Option<Instant>,
}
impl Backend {
    fn new() -> Self {
        let url = environment("WAZIG_HUB_URL");
        Self {
            names: HashMap::new(),
            names_updated: None,
            hub: if url.trim().is_empty() {
                None
            } else {
                Some(Hub {
                    base: url.trim_end_matches('/').into(),
                    token: {
                        let token = environment("WAZIG_HUB_TOKEN");
                        if token.is_empty() {
                            stored_hub_token().unwrap_or_default()
                        } else {
                            token
                        }
                    },
                })
            },
            wacli: env::var_os("WAZIG_RUST_WACLI")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(env::var_os("LOCALAPPDATA").unwrap_or_default())
                        .join("Programs/wacli/wacli.exe")
                }),
        }
    }
    fn cli(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let out = Command::new(&self.wacli)
            .args(args)
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| format!("{}: {e}", self.wacli.display()))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(300)
                .collect());
        }
        Ok(out.stdout)
    }
    fn chats(&mut self, archived: bool) -> Result<Vec<Chat>, String> {
        let bytes = if let Some(h) = &self.hub {
            h.request(
                &format!("/v1/wazig/chats?archived={archived}&limit=250"),
                None,
            )?
        } else {
            self.cli(&[
                "--json",
                "--read-only",
                "chats",
                "list",
                "--limit",
                "250",
                if archived {
                    "--archived"
                } else {
                    "--no-archived"
                },
            ])?
        };
        let mut chats = model::chats(&bytes)?;
        if self.hub.is_none() {
            if self
                .names_updated
                .is_none_or(|t| t.elapsed().as_secs() >= 60)
            {
                let mut names = HashMap::new();
                for (args, groups) in [
                    (
                        vec![
                            "--json",
                            "--read-only",
                            "contacts",
                            "search",
                            "@",
                            "--limit",
                            "10000",
                        ],
                        false,
                    ),
                    (
                        vec!["--json", "--read-only", "groups", "list", "--limit", "1000"],
                        true,
                    ),
                ] {
                    if let Ok(raw) = self.cli(&args) {
                        if let Ok(rows) = model::names(&raw, groups) {
                            names.extend(rows);
                        }
                    }
                }
                self.names = names;
                self.names_updated = Some(Instant::now());
            }
            for c in &mut chats {
                if let Some(name) = self.names.get(&c.jid) {
                    c.name.clone_from(name);
                }
            }
            model::finish_chats(&mut chats);
        }
        Ok(chats)
    }
    fn messages(&self, jid: &str) -> Result<Vec<Message>, String> {
        let bytes = if let Some(h) = &self.hub {
            h.request(
                &format!(
                    "/v1/wazig/messages?chat={}&limit=80&before=",
                    model::encode(jid)
                ),
                None,
            )?
        } else {
            self.cli(&[
                "--json",
                "--read-only",
                "messages",
                "list",
                "--chat",
                jid,
                "--limit",
                "80",
            ])?
        };
        // Deliver history before any network media work: one missing photo must not hide a chat.
        model::messages(&bytes, jid, self.hub.is_some())
    }
    fn asset(&self, jid: &str, id: &str) -> Result<String, String> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (jid, id).hash(&mut hash);
        let cache = settings_dir().join(if id.is_empty() { "avatars" } else { "media" });
        std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        let path = cache.join(format!("{:016x}", hash.finish()));
        if path.exists() {
            return Ok(path.to_string_lossy().into());
        }
        if let Some(h) = &self.hub {
            let route = if id.is_empty() {
                format!("/v1/wazig/avatar/{}", model::encode(jid))
            } else {
                format!(
                    "/v1/wazig/media/{}/{}",
                    model::encode(jid),
                    model::encode(id)
                )
            };
            std::fs::write(&path, h.request(&route, None)?).map_err(|e| e.to_string())?;
        } else if id.is_empty() {
            let raw = self.cli(&[
                "--json",
                "--lock-wait",
                "10s",
                "--timeout",
                "30s",
                "profile",
                "picture-info",
                "--jid",
                jid,
            ])?;
            let v: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            let url = v
                .pointer("/data/url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("No profile picture")?;
            // Same URLMon download as Zig; profile URLs are not hub-authenticated requests.
            unsafe {
                windows::Win32::System::Com::Urlmon::URLDownloadToFileW(
                    None,
                    PCWSTR(wide(url).as_ptr()),
                    PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                    0,
                    None,
                )
                .map_err(|e| e.to_string())?;
            }
        } else {
            let raw = self.cli(&[
                "--json",
                "--lock-wait",
                "10s",
                "--timeout",
                "60s",
                "media",
                "download",
                "--chat",
                jid,
                "--id",
                id,
            ])?;
            let v: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            return v
                .pointer("/data/path")
                .or_else(|| v.pointer("/data/LocalPath"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .ok_or_else(|| "No downloaded media path".into());
        }
        Ok(path.to_string_lossy().into())
    }
    fn send(&self, jid: &str, text: &str) -> Result<(), String> {
        if let Some(h) = &self.hub {
            let id = format!(
                "rust-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let body = json!({"chat":jid, "text":text, "reply_to":"", "client_id":id});
            // Retry once with the same id: the server may accept before dropping the response.
            let bytes = h
                .request("/v1/wazig/send", Some(&body))
                .or_else(|_| h.request("/v1/wazig/send", Some(&body)))?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if v.get("status").and_then(Value::as_str) != Some("sent") {
                return Err("Hub did not confirm sent".into());
            }
        } else {
            let args = model::send_args(jid, text);
            let bytes = self.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if v.get("success") == Some(&Value::Bool(false)) {
                return Err("wacli rejected send".into());
            }
        }
        Ok(())
    }
    fn archive(&self, jid: &str, archived: bool) -> Result<(), String> {
        if let Some(h) = &self.hub {
            h.request(
                "/v1/wazig/archive",
                Some(&json!({"chat":jid,"archived":archived})),
            )?;
        } else {
            self.cli(&[
                "--json",
                "--lock-wait",
                "10s",
                "chats",
                if archived { "archive" } else { "unarchive" },
                "--chat",
                jid,
            ])?;
        }
        Ok(())
    }
    fn mark_read(&self, jid: &str) -> Result<(), String> {
        if let Some(h) = &self.hub {
            h.request(
                "/v1/wazig/mark-read",
                Some(&json!({"chat":jid,"message_id":""})),
            )?;
        } else {
            self.cli(&[
                "--json",
                "--lock-wait",
                "70s",
                "chats",
                "mark-read",
                "--chat",
                jid,
            ])?;
        }
        Ok(())
    }
}

pub fn start(hwnd: HWND, wake: u32) -> (mpsc::Sender<Job>, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    let window = hwnd.0 as usize;
    let (assets, asset_rx) = mpsc::sync_channel::<(String, String)>(1);
    let asset_events = events.clone();
    thread::spawn(move || {
        let backend = Backend::new();
        while let Ok((jid, id)) = asset_rx.recv() {
            let result = backend.asset(&jid, &id);
            if asset_events.send(Event::Asset(jid, id, result)).is_err() {
                break;
            }
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    HWND(window as *mut _),
                    wake,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
    let (writes, write_rx) = mpsc::channel::<Job>();
    let write_events = events.clone();
    thread::spawn(move || {
        let backend = Backend::new();
        while let Ok(job) = write_rx.recv() {
            let event = match job {
                Job::Send(j, t) => {
                    let result = backend.send(&j, &t);
                    Event::Sent(j, t, result)
                }
                Job::Archive(j, a) => Event::Changed(backend.archive(&j, a)),
                Job::MarkRead(j) => {
                    let _ = backend.mark_read(&j);
                    continue;
                }
                _ => unreachable!(),
            };
            if write_events.send(event).is_err() {
                break;
            }
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    HWND(window as *mut _),
                    wake,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
    thread::spawn(move || {
        let mut backend = Backend::new();
        while let Ok(job) = rx.recv() {
            let event = match job {
                Job::Asset(jid, id) => {
                    let _ = assets.send((jid, id));
                    continue;
                }
                Job::Chats(a) => Event::Chats(a, backend.chats(a)),
                Job::Messages(j) => {
                    let result = backend.messages(&j);
                    Event::Messages(j, result)
                }
                job @ (Job::Send(..) | Job::Archive(..) | Job::MarkRead(..)) => {
                    // A busy store's write lock must never hold up read-only chat/history requests.
                    let _ = writes.send(job);
                    continue;
                }
            };
            if events.send(event).is_err() {
                break;
            }
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    HWND(window as *mut _),
                    wake,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
    (tx, receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Security::Cryptography::*;
    #[test]
    fn saved_hub_dpapi_format_is_readable_without_touching_settings() {
        unsafe {
            let mut fake = b"fixture-key-not-a-secret".to_vec();
            let input = CRYPT_INTEGER_BLOB {
                cbData: fake.len() as u32,
                pbData: fake.as_mut_ptr(),
            };
            let mut encrypted = CRYPT_INTEGER_BLOB::default();
            CryptProtectData(&input, PCWSTR::null(), None, None, None, 0, &mut encrypted).unwrap();
            let mut bytes =
                std::slice::from_raw_parts(encrypted.pbData, encrypted.cbData as usize).to_vec();
            let _ = LocalFree(HLOCAL(encrypted.pbData.cast()));
            assert_eq!(
                decrypt_token(&mut bytes).as_deref(),
                Some("fixture-key-not-a-secret")
            );
            assert!(decrypt_token(&mut [1, 2, 3]).is_none());
        }
    }
}
