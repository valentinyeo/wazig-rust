use crate::{
    backend::{self, Event, Job},
    control,
    draw::{self, Painter, GREEN, GREY, WHITE},
    model::{Chat, Message},
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::mpsc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{w, Result, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::Common::*, Gdi::*},
        System::{Com::*, LibraryLoader::*},
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const WAKE: u32 = WM_APP + 1;
const CONTROL: u32 = WM_APP + 2;
const SIDEBAR: f32 = 375.;
const COMMANDS: [(&str, &str); 5] = [
    ("Search chats", "Ctrl+F"),
    ("Compose message", "C"),
    ("Keyboard shortcuts", "F1"),
    ("Refresh", "R"),
    ("Quit", "Q"),
];
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    List,
    Search,
    Composer,
    Commands,
    Help,
}

struct App {
    hwnd: HWND,
    painter: Option<Painter>,
    jobs: mpsc::Sender<Job>,
    events: mpsc::Receiver<Event>,
    control: mpsc::Receiver<control::Request>,
    chats: Vec<Chat>,
    messages: Vec<Message>,
    selected: String,
    focus: Focus,
    search: String,
    draft: String,
    drafts: HashMap<String, String>,
    command: String,
    command_index: usize,
    list_scroll: usize,
    message_scroll: f32,
    archived: bool,
    unread: HashMap<String, bool>,
    status: String,
    last_send_status: String,
    chats_pending: bool,
    message_pending: String,
    started: Instant,
    first_paint_ms: Option<f64>,
    first_paint_unix_ms: Option<u128>,
    pending_key: Option<Instant>,
    latencies: Vec<f64>,
    paints: u64,
    visible: usize,
    dpi: f32,
    surrogate: Option<u16>,
    suppress_c: bool,
}
impl App {
    fn refresh(&mut self) {
        if !self.chats_pending {
            self.chats_pending = true;
            let _ = self.jobs.send(Job::Chats(self.archived));
        }
        self.refresh_messages();
    }
    fn refresh_messages(&mut self) {
        if !self.selected.is_empty() && self.message_pending != self.selected {
            self.message_pending = self.selected.clone();
            let _ = self.jobs.send(Job::Messages(self.selected.clone()));
        }
    }
    fn indices(&self) -> Vec<usize> {
        let search = self.search.to_lowercase();
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.name.to_lowercase().contains(&search) || c.jid.to_lowercase().contains(&search)
            })
            .map(|(i, _)| i)
            .collect()
    }
    fn select(&mut self, jid: String) {
        if let Some(next) = self
            .indices()
            .iter()
            .position(|&i| self.chats[i].jid == jid)
        {
            let rows = ((self.size().1 - 110.) / 64.).max(1.) as usize;
            if next < self.list_scroll {
                self.list_scroll = next;
            }
            if next >= self.list_scroll + rows {
                self.list_scroll = next + 1 - rows;
            }
        }
        if self.selected == jid {
            return;
        }
        self.drafts
            .insert(self.selected.clone(), std::mem::take(&mut self.draft));
        self.draft = self.drafts.remove(&jid).unwrap_or_default();
        self.selected = jid;
        self.messages.clear();
        self.visible = 0;
        self.message_scroll = 0.;
        self.refresh_messages();
        let _ = self.jobs.send(Job::MarkRead(self.selected.clone()));
    }
    fn move_chat(&mut self, delta: isize) {
        let indices = self.indices();
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|&i| self.chats[i].jid == self.selected)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, indices.len() as isize - 1) as usize;
        self.select(self.chats[indices[next]].jid.clone());
    }
    fn send(&mut self, text: String) {
        if text.trim().is_empty() || self.selected.is_empty() {
            return;
        }
        self.last_send_status = "sending".into();
        self.status = "Sending…".into();
        let _ = self.jobs.send(Job::Send(self.selected.clone(), text));
    }
    fn events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Chats(archived, result) => {
                    self.chats_pending = false;
                    if archived != self.archived {
                        self.refresh();
                        continue;
                    }
                    match result {
                        Ok(chats) => {
                            self.chats = chats;
                            self.status.clear();
                            if !self.chats.iter().any(|c| c.jid == self.selected) {
                                if let Some(c) = self.chats.first() {
                                    self.select(c.jid.clone());
                                } else {
                                    self.selected.clear();
                                    self.messages.clear();
                                }
                            }
                        }
                        Err(e) => self.status = e,
                    }
                }
                Event::Messages(jid, result) => {
                    if self.message_pending == jid {
                        self.message_pending.clear();
                    }
                    if jid == self.selected {
                        match result {
                            Ok(m) => self.messages = m,
                            Err(e) => self.status = e,
                        }
                    }
                }
                Event::Sent(jid, text, result) => match result {
                    Ok(()) => {
                        self.last_send_status = "sent".into();
                        self.status = "Sent".into();
                        if jid == self.selected {
                            self.messages.push(Message {
                                id: format!("pending-{}", self.paints),
                                chat: jid.clone(),
                                text: text.clone(),
                                sender: "You".into(),
                                from_me: true,
                                ..Default::default()
                            });
                            if self.draft == text {
                                self.draft.clear();
                            }
                            self.message_scroll = 0.;
                        } else if self.drafts.get(&jid) == Some(&text) {
                            self.drafts.remove(&jid);
                        }
                        self.refresh_messages();
                    }
                    Err(e) => {
                        self.last_send_status = "failed".into();
                        self.status = format!("Send failed: {e}");
                    }
                },
                Event::Changed(result) => match result {
                    Ok(()) => self.refresh(),
                    Err(e) => self.status = e,
                },
            }
        }
        self.invalidate();
    }
    fn state(&self) -> serde_json::Value {
        json!({"chats":self.chats.len(),"selected":self.selected,"messages_visible":self.visible,
            "messages_loaded":self.messages.len(),
            "focus":match self.focus {Focus::List=>"list",Focus::Search=>"search",Focus::Composer=>"composer",Focus::Commands=>"commands",Focus::Help=>"help"},
            "search_matches":self.indices().len(),"composer_characters":self.draft.chars().count(),"last_send_status":self.last_send_status,"status":self.status,
            "first_chat_painted_ms":self.first_paint_ms,"first_chat_painted_unix_ms":self.first_paint_unix_ms,
            "key_to_paint_ms":self.latencies,"paints":self.paints,"archived":self.archived,
            "title":format!("Wazig Rust v{}",version())})
    }
    fn control(&mut self) {
        while let Ok(request) = self.control.try_recv() {
            let result = if request.command == "state" {
                self.state()
            } else if let Some(jid) = request.command.strip_prefix("select ") {
                if self.chats.iter().any(|c| c.jid == jid) {
                    self.select(jid.into());
                    self.invalidate();
                    json!({"ok":true})
                } else {
                    json!({"ok":false,"error":"Unknown chat"})
                }
            } else if let Some(text) = request.command.strip_prefix("send ") {
                if self.selected.is_empty() || text.trim().is_empty() {
                    json!({"ok":false,"error":"Select a chat and provide text"})
                } else {
                    self.send(text.into());
                    self.invalidate();
                    json!({"ok":true})
                }
            } else {
                json!({"ok":false,"error":"Expected state, select <jid>, or send <text>"})
            };
            let _ = request.reply.send(result);
        }
    }
    fn size(&self) -> (f32, f32) {
        unsafe {
            let mut r = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut r);
            (
                (r.right - r.left) as f32 * 96. / self.dpi,
                (r.bottom - r.top) as f32 * 96. / self.dpi,
            )
        }
    }
    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }
    fn filtered_commands(&self) -> Vec<usize> {
        COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, c)| c.0.to_lowercase().contains(&self.command.to_lowercase()))
            .map(|(i, _)| i)
            .collect()
    }
    fn execute_command(&mut self, index: usize) {
        self.focus = Focus::List;
        match index {
            0 => self.focus = Focus::Search,
            1 => self.focus = Focus::Composer,
            2 => self.focus = Focus::Help,
            3 => self.refresh(),
            4 => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            _ => {}
        }
    }
    fn paste(&mut self) {
        use windows::Win32::System::{DataExchange::*, Memory::*};
        if self.focus == Focus::List || self.focus == Focus::Help {
            return;
        }
        unsafe {
            if OpenClipboard(self.hwnd).is_err() {
                return;
            }
            if let Ok(handle) = GetClipboardData(13) {
                let memory = HGLOBAL(handle.0);
                let size = GlobalSize(memory).min(128 * 1024) / 2;
                let ptr = GlobalLock(memory) as *const u16;
                if !ptr.is_null() {
                    let units = std::slice::from_raw_parts(ptr, size);
                    let end = units.iter().position(|&u| u == 0).unwrap_or(size);
                    let value = String::from_utf16_lossy(&units[..end]).replace("\r\n", "\n");
                    match self.focus {
                        Focus::Composer => self.draft.push_str(&value),
                        Focus::Search => {
                            self.search.push_str(&value.replace('\n', " "));
                            self.list_scroll = 0;
                        }
                        Focus::Commands => {
                            self.command.push_str(&value.replace('\n', " "));
                            self.command_index = 0;
                        }
                        _ => {}
                    }
                    let _ = GlobalUnlock(memory);
                }
            }
            let _ = CloseClipboard();
        }
    }
    fn key(&mut self, key: u32) {
        if key == VK_DOWN.0 as u32 {
            self.pending_key = Some(Instant::now());
        }
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        if ctrl && key == b'K' as u32 {
            self.focus = Focus::Commands;
            self.command.clear();
            self.command_index = 0;
        } else if ctrl && key == b'F' as u32 {
            self.focus = Focus::Search;
        } else if ctrl && key == b'V' as u32 {
            self.paste();
        } else if key == VK_ESCAPE.0 as u32 {
            self.focus = Focus::List;
            self.search.clear();
            self.command.clear();
        } else if key == VK_F1.0 as u32 {
            self.focus = Focus::Help;
        } else if key == VK_TAB.0 as u32 {
            self.focus = if self.focus == Focus::Composer {
                Focus::List
            } else {
                Focus::Composer
            };
        } else if self.focus == Focus::Commands {
            let count = self.filtered_commands().len();
            if key == VK_DOWN.0 as u32 && count > 0 {
                self.command_index = (self.command_index + 1) % count;
            }
            if key == VK_UP.0 as u32 && count > 0 {
                self.command_index = self.command_index.checked_sub(1).unwrap_or(count - 1);
            }
            if key == VK_RETURN.0 as u32 {
                if let Some(&i) = self.filtered_commands().get(self.command_index) {
                    self.execute_command(i);
                }
            }
        } else if key == VK_RETURN.0 as u32 {
            if self.focus == Focus::Composer {
                if shift {
                    self.draft.push('\n');
                } else if self.last_send_status != "sending" {
                    self.send(self.draft.clone());
                }
            } else {
                self.focus = Focus::Composer;
            }
        } else if self.focus == Focus::List {
            match key {
                0x28 | 0x4a => self.move_chat(1),
                0x26 | 0x4b => self.move_chat(-1),
                0x43 => {
                    self.focus = Focus::Composer;
                    self.suppress_c = true;
                }
                0x52 => self.refresh(),
                0x51 => unsafe {
                    let _ = DestroyWindow(self.hwnd);
                },
                0x45 => {
                    if !self.selected.is_empty() {
                        let _ = self
                            .jobs
                            .send(Job::Archive(self.selected.clone(), !self.archived));
                    }
                }
                0x55 => {
                    if !self.selected.is_empty() {
                        let current =
                            self.unread.get(&self.selected).copied().unwrap_or_else(|| {
                                self.chats
                                    .iter()
                                    .find(|c| c.jid == self.selected)
                                    .is_some_and(|c| c.unread_count > 0)
                            });
                        self.unread.insert(self.selected.clone(), !current);
                        if let Ok(bytes) = serde_json::to_vec(&self.unread) {
                            let _ =
                                std::fs::write(backend::settings_dir().join("unread.json"), bytes);
                        }
                    }
                }
                0x41 if ctrl => {
                    self.archived = !self.archived;
                    self.refresh();
                }
                _ => {}
            }
        }
        self.invalidate();
    }
    fn character(&mut self, unit: u16) {
        if self.suppress_c {
            self.suppress_c = false;
            if unit == b'c' as u16 || unit == b'C' as u16 {
                return;
            }
        }
        if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 {
            return;
        }
        if self.focus == Focus::List {
            if unit == b'/' as u16 {
                self.focus = Focus::Search;
                self.search.clear();
                self.invalidate();
            }
            return;
        }
        let text = match self.focus {
            Focus::Search => &mut self.search,
            Focus::Composer => &mut self.draft,
            Focus::Commands => &mut self.command,
            _ => return,
        };
        if unit == 8 {
            text.pop();
        } else if unit >= 32 {
            if (0xd800..=0xdbff).contains(&unit) {
                self.surrogate = Some(unit);
                return;
            }
            if let Some(high) = self.surrogate.take() {
                text.push_str(&String::from_utf16_lossy(&[high, unit]));
            } else if let Some(c) = char::from_u32(unit as u32) {
                text.push(c);
            }
        }
        if self.focus == Focus::Search {
            self.list_scroll = 0;
        }
        if self.focus == Focus::Commands {
            self.command_index = 0;
        }
        self.invalidate();
    }
    fn click(&mut self, x: f32, y: f32) {
        let (width, height) = self.size();
        if self.focus == Focus::Commands {
            let left = (width - 560.) / 2.;
            let top = (height - 360.) / 2.;
            if x >= left && x <= left + 560. && y >= top + 70. {
                let row = ((y - top - 70.) / 48.) as usize;
                if let Some(&i) = self.filtered_commands().get(row) {
                    self.execute_command(i);
                }
            }
        } else if x < SIDEBAR {
            if y < 70. {
                self.focus = Focus::Search;
            } else if y < height - 40. {
                let row = ((y - 78.) / 64.).max(0.) as usize + self.list_scroll;
                if let Some(&i) = self.indices().get(row) {
                    self.select(self.chats[i].jid.clone());
                    self.focus = Focus::List;
                }
            }
        } else if y > height - 90. {
            self.focus = Focus::Composer;
            if x > width - 75. && self.last_send_status != "sending" {
                self.send(self.draft.clone());
            }
        } else {
            self.focus = Focus::List;
        }
        self.invalidate();
    }
    unsafe fn paint(&mut self) {
        let mut ps = PAINTSTRUCT::default();
        let _ = BeginPaint(self.hwnd, &mut ps);
        let (width, height) = self.size();
        if self.painter.is_none() {
            self.painter = Painter::new(
                self.hwnd,
                (width * self.dpi / 96.) as u32,
                (height * self.dpi / 96.) as u32,
                self.dpi,
            )
            .ok();
        }
        if let Some(mut p) = self.painter.take() {
            p.target.BeginDraw();
            p.target.Clear(Some(&D2D1_COLOR_F {
                r: 0.,
                g: 0.,
                b: 0.,
                a: 1.,
            }));
            self.draw(&mut p, width, height);
            if p.target.EndDraw(None, None).is_ok() {
                self.paints += 1;
                if !self.chats.is_empty() && self.first_paint_ms.is_none() {
                    self.first_paint_ms = Some(self.started.elapsed().as_secs_f64() * 1000.);
                    self.first_paint_unix_ms = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    );
                }
                if let Some(start) = self.pending_key.take() {
                    if self.latencies.len() >= 512 {
                        self.latencies.remove(0);
                    }
                    self.latencies.push(start.elapsed().as_secs_f64() * 1000.);
                }
                self.painter = Some(p);
            } else {
                self.invalidate();
            }
        }
        let _ = EndPaint(self.hwnd, &ps);
    }
    unsafe fn draw(&mut self, p: &mut Painter, width: f32, height: f32) {
        use draw::rect;
        p.fill(rect(SIDEBAR - 1., 0., 1., height), 0., 0x202020);
        p.fill(rect(16., 18., SIDEBAR - 32., 43.), 22., 0x1e1e1e);
        let search = if self.search.is_empty() {
            "Search WhatsApp personal  Ctrl+F"
        } else {
            &self.search
        };
        p.text(
            search,
            rect(32., 29., SIDEBAR - 60., 28.),
            14.,
            false,
            if self.focus == Focus::Search {
                WHITE
            } else {
                GREY
            },
        );
        p.target.PushAxisAlignedClip(
            &rect(0., 76., SIDEBAR, height - 120.),
            windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        );
        for (row, i) in self
            .indices()
            .into_iter()
            .skip(self.list_scroll)
            .enumerate()
        {
            let y = 78. + row as f32 * 64.;
            if y > height - 40. {
                break;
            }
            let c = &self.chats[i];
            if c.jid == self.selected {
                p.fill(rect(10., y, SIDEBAR - 20., 60.), 12., 0x252525);
            }
            p.circle(42., y + 30., 22., 0x3a3a3a);
            p.text(
                &c.name
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string(),
                rect(33., y + 15., 25., 30.),
                21.,
                true,
                WHITE,
            );
            p.circle(57., y + 46., 9., GREEN);
            p.text("◔", rect(51., y + 37., 14., 18.), 14., true, 0x000000);
            let name = if c.name.is_empty() { &c.jid } else { &c.name };
            p.text(
                name,
                rect(78., y + 9., SIDEBAR - 100., 25.),
                16.,
                true,
                WHITE,
            );
            p.text(
                if c.kind == "group" || c.jid.ends_with("@g.us") {
                    "Group"
                } else {
                    "dm"
                },
                rect(78., y + 34., 200., 20.),
                13.,
                false,
                GREY,
            );
            if self
                .unread
                .get(&c.jid)
                .copied()
                .unwrap_or(c.unread_count > 0)
            {
                p.circle(SIDEBAR - 30., y + 42., 4., GREEN);
            }
        }
        p.target.PopAxisAlignedClip();
        p.text(
            if self.archived {
                "Showing archived WhatsApp personal"
            } else {
                "Showing WhatsApp personal"
            },
            rect(18., height - 30., SIDEBAR - 30., 22.),
            12.,
            false,
            GREY,
        );
        if let Some(c) = self.chats.iter().find(|c| c.jid == self.selected) {
            p.text(
                &c.name,
                rect(SIDEBAR + 30., 18., width - SIDEBAR - 60., 30.),
                18.,
                true,
                WHITE,
            );
        }
        let area_width = (width - SIDEBAR - 80.).max(120.);
        let bubble_width = 620f32.min(area_width);
        let text_width = bubble_width - 32.;
        let mut heights = Vec::with_capacity(self.messages.len());
        for (i, m) in self.messages.iter().enumerate() {
            let sender = i == 0
                || self.messages[i - 1].sender != m.sender
                || self.messages[i - 1].from_me != m.from_me;
            let body = m.quote().map(|(_, b)| b).unwrap_or_else(|| m.body());
            let text_h = if body.is_empty() {
                0.
            } else {
                p.height(body, text_width, 16., false)
            };
            let h = 24.
                + text_h
                + 18.
                + if sender { 26. } else { 0. }
                + if m.quote().is_some() { 64. } else { 0. }
                + if m.media_type == "image" { 220. } else { 0. };
            heights.push((h, sender, if m.reaction.is_empty() { 14. } else { 32. }));
        }
        let total: f32 = heights.iter().map(|(h, _, gap)| h + gap).sum();
        self.message_scroll = self
            .message_scroll
            .clamp(0., (total - (height - 180.)).max(0.));
        let mut y = (height - 120. - total).min(70.) + self.message_scroll;
        self.visible = 0;
        p.target.PushAxisAlignedClip(
            &rect(SIDEBAR + 1., 64., width - SIDEBAR - 1., height - 180.),
            windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        );
        for (m, (h, sender, gap)) in self.messages.iter().zip(heights) {
            let x = if m.from_me {
                width - bubble_width - 36.
            } else {
                SIDEBAR + 36.
            };
            if y + h >= 64. && y < height - 116. {
                self.visible += 1;
                p.fill(
                    rect(x, y, bubble_width, h),
                    16.,
                    if m.from_me { 0x16271e } else { 0x1e1e1e },
                );
                let mut content_y = y + 12.;
                if sender {
                    p.text(
                        if m.from_me { "You" } else { &m.sender },
                        rect(x + 16., content_y, text_width, 25.),
                        15.,
                        true,
                        WHITE,
                    );
                    content_y += 26.;
                }
                if let Some((quote, _)) = m.quote() {
                    p.fill(
                        rect(x + 12., content_y, bubble_width - 24., 56.),
                        6.,
                        0x30302b,
                    );
                    p.fill(rect(x + 12., content_y, 3., 56.), 1., 0xd6c277);
                    let name = self
                        .messages
                        .iter()
                        .find(|q| q.id == m.quoted_msg_id)
                        .map(|q| if q.from_me { "You" } else { q.sender.as_str() })
                        .unwrap_or(&m.quoted_sender_jid);
                    p.text(
                        name,
                        rect(x + 24., content_y + 4., text_width - 20., 20.),
                        13.,
                        true,
                        0xd6c277,
                    );
                    p.text(
                        quote,
                        rect(x + 24., content_y + 25., text_width - 20., 25.),
                        14.,
                        false,
                        GREY,
                    );
                    content_y += 64.;
                }
                if m.media_type == "image" {
                    if m.local_path.is_empty()
                        || !p.image(&m.local_path, rect(x + 16., content_y, text_width, 210.))
                    {
                        p.fill(rect(x + 16., content_y, text_width, 210.), 8., 0x2c2c2c);
                        p.text(
                            "Image loading…",
                            rect(x + 30., content_y + 80., 250., 30.),
                            15.,
                            false,
                            GREY,
                        );
                    }
                    content_y += 220.;
                }
                let body = m.quote().map(|(_, b)| b).unwrap_or_else(|| m.body());
                p.text(
                    body,
                    rect(x + 16., content_y, text_width, h - (content_y - y) - 20.),
                    16.,
                    false,
                    WHITE,
                );
                p.text(
                    m.time(),
                    rect(x + bubble_width - 60., y + h - 22., 48., 18.),
                    11.,
                    false,
                    GREY,
                );
                if !m.reaction.is_empty() {
                    p.fill(rect(x + 14., y + h - 5., 52., 25.), 12., 0x333333);
                    p.text(
                        &m.reaction,
                        rect(x + 24., y + h - 4., 40., 24.),
                        15.,
                        false,
                        WHITE,
                    );
                }
            }
            y += h + gap;
        }
        p.target.PopAxisAlignedClip();
        p.fill(
            rect(SIDEBAR + 24., height - 88., width - SIDEBAR - 48., 62.),
            31.,
            0x202020,
        );
        p.text(
            "+",
            rect(SIDEBAR + 43., height - 76., 30., 40.),
            28.,
            false,
            GREY,
        );
        let draft = if self.draft.is_empty() {
            "Message"
        } else {
            &self.draft
        };
        p.text(
            draft,
            rect(SIDEBAR + 86., height - 69., width - SIDEBAR - 202., 37.),
            16.,
            false,
            if self.draft.is_empty() { GREY } else { WHITE },
        );
        p.text(
            "☺",
            rect(width - 122., height - 72., 26., 35.),
            23.,
            false,
            GREY,
        );
        p.circle(width - 60., height - 57., 21., GREEN);
        p.text(
            "➤",
            rect(width - 72., height - 73., 30., 35.),
            24.,
            false,
            0x07150c,
        );
        if self.focus == Focus::Composer {
            p.fill(rect(SIDEBAR + 86., height - 40., 2., 12.), 0., GREEN);
        }
        p.text(
            &self.status,
            rect(SIDEBAR + 30., height - 22., width - SIDEBAR - 60., 20.),
            11.,
            false,
            GREY,
        );
        if self.focus == Focus::Commands || self.focus == Focus::Help {
            let left = (width - 560.) / 2.;
            let top = (height - 360.) / 2.;
            p.fill(rect(left - 2., top - 2., 564., 364.), 18., 0x424242);
            p.fill(rect(left, top, 560., 360.), 16., 0x1b1b1b);
            if self.focus == Focus::Commands {
                p.text(
                    if self.command.is_empty() {
                        "Type a command"
                    } else {
                        &self.command
                    },
                    rect(left + 24., top + 22., 510., 35.),
                    20.,
                    false,
                    WHITE,
                );
                for (row, i) in self.filtered_commands().into_iter().enumerate() {
                    let y = top + 70. + row as f32 * 48.;
                    if row == self.command_index {
                        p.fill(rect(left + 10., y, 540., 44.), 8., 0x333333);
                    }
                    p.text(
                        COMMANDS[i].0,
                        rect(left + 24., y + 11., 360., 30.),
                        16.,
                        false,
                        WHITE,
                    );
                    p.text(
                        COMMANDS[i].1,
                        rect(left + 450., y + 12., 90., 25.),
                        13.,
                        false,
                        GREY,
                    );
                }
            } else {
                p.text(
                    "Keyboard shortcuts",
                    rect(left + 24., top + 22., 510., 35.),
                    20.,
                    true,
                    WHITE,
                );
                p.text("↑/↓ or J/K   Move chats\nEnter / C   Compose     Enter   Send\nShift+Enter   New line     Esc   Back\nCtrl+F / /   Search     Ctrl+K   Commands\nE   Archive / unarchive     U   Toggle unread\nCtrl+A   View archived chats\nR   Refresh     Q   Quit",rect(left+24.,top+75.,510.,270.),17.,false,WHITE);
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let app = &mut *ptr;
    match msg {
        WM_PAINT => {
            app.paint();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WAKE => {
            app.events();
            LRESULT(0)
        }
        CONTROL => {
            app.control();
            LRESULT(0)
        }
        WM_TIMER => {
            app.refresh();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            app.key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            app.character(wparam.0 as u16);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            app.click(
                (lparam.0 as i16) as f32 * 96. / app.dpi,
                ((lparam.0 >> 16) as i16) as f32 * 96. / app.dpi,
            );
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as i16) as f32;
            let mut point = POINT {
                x: lparam.0 as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            let _ = ScreenToClient(hwnd, &mut point);
            if point.x as f32 * 96. / app.dpi < SIDEBAR {
                let max = app.indices().len().saturating_sub(1);
                app.list_scroll = (app.list_scroll as isize - if delta > 0. { 3 } else { -3 })
                    .clamp(0, max as isize) as usize;
            } else {
                app.message_scroll = (app.message_scroll + delta).max(0.);
            }
            app.invalidate();
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(p) = &app.painter {
                let _ = p.target.Resize(&D2D_SIZE_U {
                    width: (lparam.0 as u32) & 0xffff,
                    height: ((lparam.0 as u32) >> 16) & 0xffff,
                });
            }
            app.invalidate();
            LRESULT(0)
        }
        WM_DPICHANGED => {
            app.dpi = (wparam.0 & 0xffff) as f32;
            app.painter = None;
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            app.invalidate();
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
fn version() -> &'static str {
    option_env!("WAZIG_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn run() -> Result<()> {
    let started = Instant::now();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let instance = GetModuleHandleW(None)?;
        let class = w!("WazigRust");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let title: Vec<u16> = format!("Wazig Rust v{}", version())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            900,
            None,
            None,
            instance,
            None,
        )?;
        let dark = BOOL(1);
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const BOOL).cast(),
            std::mem::size_of_val(&dark) as u32,
        );
        let (jobs, events) = backend::start(hwnd, WAKE);
        let control = control::start(hwnd, CONTROL);
        let _ = std::fs::create_dir_all(backend::settings_dir());
        let unread = std::fs::read(backend::settings_dir().join("unread.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut app = Box::new(App {
            hwnd,
            painter: None,
            jobs,
            events,
            control,
            chats: Vec::new(),
            messages: Vec::new(),
            selected: String::new(),
            focus: Focus::List,
            search: String::new(),
            draft: String::new(),
            drafts: HashMap::new(),
            command: String::new(),
            command_index: 0,
            list_scroll: 0,
            message_scroll: 0.,
            archived: false,
            unread,
            status: "Loading WhatsApp…".into(),
            last_send_status: "idle".into(),
            chats_pending: false,
            message_pending: String::new(),
            started,
            first_paint_ms: None,
            first_paint_unix_ms: None,
            pending_key: None,
            latencies: Vec::new(),
            paints: 0,
            visible: 0,
            dpi: GetDpiForWindow(hwnd) as f32,
            surrogate: None,
            suppress_c: false,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *app as *mut App) as isize);
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        app.refresh();
        SetTimer(hwnd, 1, 5000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(app);
        CoUninitialize();
        Ok(())
    }
}
