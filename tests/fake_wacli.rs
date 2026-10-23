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
    if args[1] == "--read-only" && args[2] == "chats" {
        assert_eq!(
            &args[2..],
            ["chats", "list", "--limit", "250", "--no-archived"]
        );
        println!(
            r#"{{"success":true,"data":[{{"jid":"15550000999@s.whatsapp.net","name":"Fixture Local Store","kind":"direct"}}]}}"#
        );
    } else if args[1] == "--read-only" && args[2] == "messages" {
        assert_eq!(
            &args[2..],
            [
                "messages",
                "list",
                "--chat",
                "15550000999@s.whatsapp.net",
                "--limit",
                "80"
            ]
        );
        let sent = fs::read_to_string(store.join("sent.txt")).ok();
        let outgoing = sent
            .map(|s| format!(r#"{{"MsgID":"SENT","Text":{},"FromMe":true}},"#, quoted(&s)))
            .unwrap_or_default();
        println!(
            r#"{{"success":true,"data":{{"messages":[{}{{"MsgID":"BASE","Text":"Synthetic existing wacli store","SenderName":"Fixture Local"}}]}}}}"#,
            outgoing
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
                "15550000999@s.whatsapp.net"
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
                "15550000999@s.whatsapp.net"
            ]
        );
        println!(r#"{{"success":true}}"#);
    }
}
