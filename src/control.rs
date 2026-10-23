use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::windows::io::FromRawHandle,
    sync::mpsc,
    thread,
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*, Storage::FileSystem::PIPE_ACCESS_DUPLEX, System::Pipes::*,
        UI::WindowsAndMessaging::PostMessageW,
    },
};

pub struct Request {
    pub command: String,
    pub reply: mpsc::Sender<Value>,
}

pub fn start(hwnd: HWND, wake: u32) -> mpsc::Receiver<Request> {
    let (tx, rx) = mpsc::channel::<Request>();
    let window = hwnd.0 as usize;
    thread::spawn(move || unsafe {
        let name: Vec<u16> = format!("\\\\.\\pipe\\WazigRust-{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        loop {
            let pipe = CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                None,
            );
            if pipe == INVALID_HANDLE_VALUE {
                break;
            }
            let mut file = File::from_raw_handle(pipe.0);
            if ConnectNamedPipe(pipe, None).is_err() && GetLastError() != ERROR_PIPE_CONNECTED {
                continue;
            }
            let mut command = String::new();
            if BufReader::new((&mut file).take(65537))
                .read_line(&mut command)
                .is_ok()
                && command.len() <= 65536
            {
                let (reply, result) = mpsc::channel();
                if tx
                    .send(Request {
                        command: command.trim_end_matches(['\r', '\n']).into(),
                        reply,
                    })
                    .is_err()
                {
                    break;
                }
                let _ = PostMessageW(HWND(window as *mut _), wake, WPARAM(0), LPARAM(0));
                if let Ok(value) = result.recv_timeout(Duration::from_secs(5)) {
                    let _ = writeln!(file, "{value}");
                    let _ = file.flush();
                }
            }
            let _ = DisconnectNamedPipe(pipe);
        }
    });
    rx
}
