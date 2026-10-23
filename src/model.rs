use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Chat {
    pub jid: String,
    pub name: String,
    pub kind: String,
    pub provider: String,
    pub account: String,
    pub unread_count: u32,
    pub archived: bool,
    pub last_message_ts: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Message {
    #[serde(rename = "MsgID")]
    pub id: String,
    #[serde(rename = "ChatJID")]
    pub chat: String,
    #[serde(rename = "Text")]
    pub text: String,
    #[serde(rename = "DisplayText")]
    pub display: String,
    #[serde(rename = "SenderName")]
    pub sender: String,
    #[serde(rename = "FromMe")]
    pub from_me: bool,
    #[serde(rename = "Timestamp")]
    pub timestamp: String,
    #[serde(rename = "MediaType")]
    pub media_type: String,
    #[serde(rename = "LocalPath")]
    pub local_path: String,
    pub quoted_msg_id: String,
    pub quoted_sender_jid: String,
    #[serde(rename = "ReactionEmoji")]
    pub reaction: String,
    #[serde(rename = "ReactionToID")]
    pub reaction_to: String,
    #[serde(rename = "Revoked")]
    pub revoked: bool,
}

impl Message {
    pub fn body(&self) -> &str {
        if self.revoked { "Message deleted" }
        else if self.display.is_empty() { &self.text } else { &self.display }
    }
    pub fn quote(&self) -> Option<(&str, &str)> {
        if self.quoted_msg_id.is_empty() { return None; }
        let (quote, body) = self.body().split_once("\n\n")?;
        Some((quote.trim_start_matches('>').trim(), body))
    }
    pub fn time(&self) -> &str { self.timestamp.get(11..16).unwrap_or("") }
}

fn rows(raw: &[u8], messages: bool) -> Result<Value, String> {
    let mut v: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if v.get("success") == Some(&Value::Bool(false)) { return Err("Backend rejected request".into()); }
    if let Some(data) = v.get_mut("data") { v = data.take(); }
    if messages { if let Some(m) = v.get_mut("messages") { v = m.take(); } }
    if !v.is_array() { return Err("Backend did not return an array".into()); }
    Ok(v)
}

pub fn chats(raw: &[u8]) -> Result<Vec<Chat>, String> {
    let mut chats: Vec<Chat> = serde_json::from_value(rows(raw, false)?).map_err(|e| e.to_string())?;
    chats.retain(|c| !c.jid.is_empty() && (c.provider.is_empty() || c.provider == "whatsapp"));
    chats.sort_by(|a, b| b.last_message_ts.cmp(&a.last_message_ts));
    Ok(chats)
}

pub fn messages(raw: &[u8], chat: &str, hub: bool) -> Result<Vec<Message>, String> {
    let mut messages: Vec<Message> = serde_json::from_value(rows(raw, true)?).map_err(|e| e.to_string())?;
    if hub && messages.iter().any(|m| m.chat != chat) { return Err("Hub returned another chat's messages".into()); }
    if hub { for m in &mut messages { m.local_path.clear(); } }
    let reactions: Vec<_> = messages.iter().filter(|m| !m.reaction_to.is_empty())
        .map(|m| (m.reaction_to.clone(), m.reaction.clone())).collect();
    messages.retain(|m| !m.id.is_empty() && m.reaction_to.is_empty());
    messages.reverse(); // wacli and hub histories are newest first.
    for (id, reaction) in reactions {
        if let Some(m) = messages.iter_mut().find(|m| m.id == id) { m.reaction = reaction; }
    }
    Ok(messages)
}

pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { out.push(b as char); }
        else { use std::fmt::Write; let _ = write!(out, "%{b:02X}"); }
    }
    out
}

pub fn send_args(jid: &str, text: &str) -> Vec<String> {
    ["--json", "--lock-wait", "0s", "send", "text", "--to", jid, "--message", text]
        .into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_send_argv_and_unicode() {
        assert_eq!(send_args("15550000001@s.whatsapp.net", "Hello 👋\nline")[..5],
            ["--json", "--lock-wait", "0s", "send", "text"]);
        assert_eq!(send_args("j", "hello").last().unwrap(), "hello");
        assert_eq!(encode("a:@ 👋"), "a%3A%40%20%F0%9F%91%8B");
    }
    #[test]
    fn both_chat_formats_and_provider_filter() {
        let raw = br#"[{"jid":"fake","name":"Fake","provider":"whatsapp"},{"jid":"not-wa","provider":"slack"}]"#;
        assert_eq!(chats(raw).unwrap().len(), 1);
        let wrapped = format!("{{\"success\":true,\"data\":{}}}", String::from_utf8_lossy(raw));
        assert_eq!(chats(wrapped.as_bytes()).unwrap().len(), 1);
        assert!(chats(br#"{"success":false,"data":[]}"#).is_err());
    }
    #[test]
    fn history_order_quotes_and_reactions() {
        let raw = br#"{"data":{"messages":[{"MsgID":"r","ReactionToID":"old","ReactionEmoji":"ok"},{"MsgID":"new","ChatJID":"j","Text":"new"},{"MsgID":"old","ChatJID":"j","Text":"body","DisplayText":"> quote\n\nbody","quoted_msg_id":"q"}]}}"#;
        let m = messages(raw, "j", false).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "old");
        assert_eq!(m[0].reaction, "ok");
        assert_eq!(m[0].quote(), Some(("quote", "body")));
        assert!(messages(raw, "other", true).is_err());
    }
    #[test]
    fn hub_paths_are_not_trusted() {
        let m = messages(br#"[{"MsgID":"x","ChatJID":"j","LocalPath":"C:\\private"}]"#, "j", true).unwrap();
        assert!(m[0].local_path.is_empty());
    }
}
