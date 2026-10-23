#![windows_subsystem = "windows"]
mod backend;
mod control;
mod draw;
mod model;
mod ui;

fn main() {
    if let Err(error) = ui::run() {
        unsafe {
            let text: Vec<u16> = error.to_string().encode_utf16().chain(Some(0)).collect();
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(None, windows::core::PCWSTR(text.as_ptr()),
                windows::core::w!("Wazig Rust"), windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR);
        }
        std::process::exit(1);
    }
}
