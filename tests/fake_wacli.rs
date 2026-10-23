// Hosted-only executable fixture. It deliberately uses wacli's default store path.
use std::{env, fs, path::PathBuf};

fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn main() {
    assert_eq!(env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
    let args: Vec<_> = env::args().skip(1).collect();
    let store = PathBuf::from(env::var_os("USERPROFILE").unwrap()).join(".wacli");
    fs::create_dir_all(&store).unwrap();
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(store.join("argv.jsonl"))
        .unwrap();
    use std::io::Write;
    writeln!(
        log,
        "[{}]",
        args.iter().map(|s| quoted(s)).collect::<Vec<_>>().join(",")
    )
    .unwrap();
    assert_eq!(args[0], "--json");
    if args[1] == "--read-only" && args[2] == "contacts" {
        assert_eq!(&args[2..], ["contacts", "search", "@", "--limit", "10000"]);
        println!(
            r#"{{"success":true,"data":[{{"jid":"15550000999@s.whatsapp.net","alias":"Fixture Local Store","system_name":"Ignored System","name":"Ignored Push"}},{{"jid":"15550000997@s.whatsapp.net","system_name":"Fixture System Name","name":"Ignored Push"}},{{"jid":"15550000998@s.whatsapp.net","name":"Fixture Push Name"}}]}}"#
        );
    } else if args[1] == "--read-only" && args[2] == "groups" {
        assert_eq!(&args[2..], ["groups", "list", "--limit", "1000"]);
        println!(
            r#"{{"success":true,"data":[{{"JID":"120363000000099@g.us","Name":"Fixture Official Group"}}]}}"#
        );
    } else if args.get(5).map(String::as_str) == Some("profile") {
        assert_eq!(
            &args[..8],
            [
                "--json",
                "--lock-wait",
                "10s",
                "--timeout",
                "30s",
                "profile",
                "picture-info",
                "--jid"
            ]
        );
        let url = format!("{}/fixture/avatar", env::var("WAZIG_FIXTURE_URL").unwrap());
        println!(r#"{{"success":true,"data":{{"url":{}}}}}"#, quoted(&url));
    } else if args.get(5).map(String::as_str) == Some("media") {
        assert_eq!(
            &args[..8],
            [
                "--json",
                "--lock-wait",
                "10s",
                "--timeout",
                "60s",
                "media",
                "download",
                "--chat"
            ]
        );
        // A slow/unavailable real photo used to prevent the entire history from being delivered.
        std::thread::sleep(std::time::Duration::from_secs(10));
        eprintln!("Synthetic media unavailable");
        std::process::exit(1);
    } else if args[1] == "--read-only" && args[2] == "chats" {
        assert_eq!(
            &args[2..],
            ["chats", "list", "--limit", "250", "--no-archived"]
        );
        println!(
            r#"{{"success":true,"data":[{{"jid":"15550000998@s.whatsapp.net","name":"15550000998@s.whatsapp.net","kind":"dm","last_message_ts":"2026-01-02T00:00:00Z"}},{{"jid":"120363000000099@g.us","name":"Stale Group","kind":"group","last_message_ts":"2026-01-03T00:00:00Z"}},{{"jid":"15550000999@s.whatsapp.net","name":"15550000999@s.whatsapp.net","kind":"dm","pinned":true,"last_message_ts":"2020-01-01T00:00:00Z"}},{{"jid":"15550000997@s.whatsapp.net","name":"15550000997@s.whatsapp.net","kind":"dm","last_message_ts":"2026-01-02T00:00:00Z"}},{{"jid":"0@s.whatsapp.net","kind":"unknown"}},{{"jid":"status@broadcast","name":"Status"}},{{"jid":"opaque-junk","name":"opaque-junk","kind":"unknown"}}]}}"#
        );
    } else if args[1] == "--read-only" && args[2] == "messages" {
        assert_eq!(
            &args[2..],
            ["messages", "list", "--chat", &args[5], "--limit", "80"]
        );
        let sent = fs::read_to_string(store.join("sent.txt")).ok();
        let outgoing = sent
            .map(|s| format!(r#"{{"MsgID":"SENT","Text":{},"FromMe":true}},"#, quoted(&s)))
            .unwrap_or_default();
        println!(
            r#"{{"success":true,"data":{{"messages":[{}{{"MsgID":"SLOW_IMAGE","Text":"Slow synthetic image","MediaType":"image","Timestamp":"2026-01-02T12:00:00Z"}},{{"MsgID":"BASE","ChatJID":{},"Text":"Synthetic existing wacli store","SenderName":"Fixture Local","Timestamp":"2026-01-02T11:00:00Z"}}],"has_more":true,"next_cursor":"synthetic-unused-older-page"}}}}"#,
            outgoing,
            quoted(&args[5])
        );
    } else if args.get(3).map(String::as_str) == Some("send") {
        assert_eq!(
            &args[..7],
            [
                "--json",
                "--lock-wait",
                "0s",
                "send",
                "text",
                "--to",
                &args[6]
            ]
        );
        assert_eq!(args[7], "--message");
        assert_eq!(args.len(), 9);
        fs::write(store.join("sent.txt"), &args[8]).unwrap();
        println!(r#"{{"success":true,"data":{{"MsgID":"SENT"}}}}"#);
    } else {
        assert_eq!(
            &args[..],
            [
                "--json",
                "--lock-wait",
                "70s",
                "chats",
                "mark-read",
                "--chat",
                &args[6]
            ]
        );
        if fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(store.join("slow-mark-read"))
            .is_ok()
        {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        println!(r#"{{"success":true}}"#);
    }
}
