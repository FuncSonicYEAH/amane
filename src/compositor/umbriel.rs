use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::Workspace;

// umbriel sets UMBRIEL_SOCKET for the programs it starts; without it, the socket sits next to the wayland one
fn socket() -> Option<PathBuf> {
    if let Some(path) = env::var_os("UMBRIEL_SOCKET").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let runtime = env::var_os("XDG_RUNTIME_DIR")?;
    let display = env::var_os("WAYLAND_DISPLAY")?;

    let mut name = OsString::from("umbriel-");
    name.push(display);
    name.push(".sock");

    let path = PathBuf::from(runtime).join(name);

    let socket = std::fs::metadata(&path).is_ok_and(|metadata| metadata.file_type().is_socket());

    socket.then_some(path)
}

pub fn available() -> bool {
    socket().is_some()
}

fn connect() -> Option<UnixStream> {
    UnixStream::connect(socket()?).ok()
}

// one request per connection: umbriel answers the line, then closes it unless the request subscribed to events
fn request(payload: &Value) -> Option<Value> {
    let mut stream = connect()?;

    let line = format!("{payload}\n");

    stream.write_all(line.as_bytes()).ok()?;

    let mut reply = String::new();

    BufReader::new(stream).read_line(&mut reply).ok()?;

    let reply: Value = serde_json::from_str(&reply).ok()?;

    // errors carry no data, and the caller has nothing to do about them
    if reply.get("err").is_some() {
        return None;
    }

    reply.get("ok").cloned()
}

fn subscribe() -> Option<UnixStream> {
    let mut stream = connect()?;

    let request = json!({
        "cmd": "subscribe",
        "events": ["workspaces"],
    });

    stream.write_all(format!("{request}\n").as_bytes()).ok()?;

    // the first line is already the current list, with no reply before it
    Some(stream)
}

// follows umbriel's workspace snapshots until it exits
pub fn listen(mut on_change: impl FnMut(Vec<Workspace>)) {
    let Some(stream) = subscribe() else {
        return;
    };

    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };

        if let Some(list) = parse_event(&line) {
            on_change(list);
        }
    }
}

pub fn focus_workspace(id: i64) {
    // positions and names change, so the target is looked up in a fresh list
    let Some(list) = request(&json!({ "cmd": "workspaces" })).and_then(|ok| parse_list(&ok)) else {
        return;
    };

    let Some(action) = focus_payload(id, &list) else {
        return;
    };

    let _ = request(&action);
}

fn focus_payload(id: i64, list: &[Workspace]) -> Option<Value> {
    let workspace = list.iter().find(|workspace| workspace.id == id)?;

    /*
     * a string selects a named workspace even when it looks like a number;
     * unnamed workspaces are selected by their position on the output
     */
    let selector = match &workspace.name {
        Some(name) => json!(name),
        None => json!(workspace.index),
    };

    let mut payload = json!({
        "cmd": "action",
        "action": "workspace-switch",
        "workspace": selector,
    });

    if let Some(output) = &workspace.output {
        payload["output"] = json!(output);
    }

    Some(payload)
}

fn parse_event(line: &str) -> Option<Vec<Workspace>> {
    let value: Value = serde_json::from_str(line).ok()?;

    // other families are only sent when something subscribes to them
    if value["event"].as_str()? != "workspaces" {
        return None;
    }

    parse_list(&value["data"])
}

fn parse_list(data: &Value) -> Option<Vec<Workspace>> {
    let mut list = Vec::new();

    for value in data.as_array()? {
        let id = value["id"].as_str()?;
        let name = value["name"].as_str()?;

        list.push(Workspace {
            id: workspace_id(id),

            // umbriel counts positions from 1 already
            index: u32::try_from(value["index"].as_u64()?).ok()?,

            // numeric labels for workspaces the user never named
            name: (value["named"].as_bool()? && !name.is_empty()).then(|| name.to_string()),

            output: value["output"]
                .as_str()
                .filter(|output| !output.is_empty())
                .map(String::from),

            active: value["active"].as_bool()?,
            focused: value["focused"].as_bool()?,
            urgent: value["urgent"].as_bool()?,

            windows: u32::try_from(value["window_count"].as_u64()?).ok()?,
        });
    }

    Some(list)
}

/*
 * umbriel's workspace ids are strings like "DP-1:3", while Workspace::id is an i64;
 * fnv-1a gives the same string the same number on every parse, and focus finds it
 * again by asking umbriel for a fresh list
 */
fn workspace_id(id: &str) -> i64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;

    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    hash as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // trimmed from umbriel's workspaces event, with one named workspace added
    const WORKSPACES: &str = r#"{"event":"workspaces","data":[
        {"active":true,"focused":true,"focused_window":"a1","id":"VGA-1:1","index":1,"layout":"scrolling","layout_override":false,"name":"1","named":false,"occupied":true,"output":"VGA-1","urgent":false,"window_count":2},
        {"active":false,"focused":false,"focused_window":"","id":"VGA-1:8","index":2,"layout":"scrolling","layout_override":false,"name":"web","named":true,"occupied":false,"output":"VGA-1","urgent":true,"window_count":0}
    ]}"#;

    #[test]
    fn reads_workspace_snapshots() {
        let list = parse_event(WORKSPACES).expect("failed to parse umbriel workspaces");

        assert_eq!(list.len(), 2);

        assert_eq!(list[0].index, 1);
        assert_eq!(list[0].output.as_deref(), Some("VGA-1"));
        assert_eq!(list[0].name, None);
        assert_eq!(list[0].windows, 2);
        assert!(list[0].active);
        assert!(list[0].focused);

        assert_eq!(list[1].name.as_deref(), Some("web"));
        assert_eq!(list[1].windows, 0);
        assert!(list[1].urgent);
        assert!(!list[1].focused);

        assert_ne!(list[0].id, list[1].id);
    }

    #[test]
    fn keeps_workspace_ids_when_positions_change() {
        let original = parse_event(WORKSPACES).unwrap();

        let moved = WORKSPACES.replace(r#""index":2"#, r#""index":3"#);
        let moved = parse_event(&moved).unwrap();

        assert_eq!(original[1].id, moved[1].id);
        assert_eq!(moved[1].index, 3);
    }

    #[test]
    fn skips_other_events_and_bad_snapshots() {
        assert!(parse_event(r#"{"event":"windows","data":[]}"#).is_none());
        assert!(parse_event("not json").is_none());
        assert!(parse_event(r#"{"ok":[]}"#).is_none());
        assert!(parse_event(r#"{"event":"workspaces","data":[{"id":"DP-1:1"}]}"#).is_none());
    }

    #[test]
    fn focuses_named_workspaces_by_name_and_others_by_position() {
        let list = parse_event(WORKSPACES).unwrap();

        let action = focus_payload(list[0].id, &list).expect("failed to build action");

        assert_eq!(
            action,
            json!({
                "cmd": "action",
                "action": "workspace-switch",
                "workspace": 1,
                "output": "VGA-1",
            })
        );

        let action = focus_payload(list[1].id, &list).expect("failed to build action");

        assert_eq!(
            action,
            json!({
                "cmd": "action",
                "action": "workspace-switch",
                "workspace": "web",
                "output": "VGA-1",
            })
        );

        assert!(focus_payload(i64::MAX, &list).is_none());
    }

    #[test]
    fn selects_a_numeric_name_instead_of_a_position() {
        let line = r#"{"event":"workspaces","data":[
            {"active":true,"focused":true,"focused_window":"","id":"DP-1:9","index":2,"layout":"stacking","layout_override":false,"name":"2","named":true,"occupied":false,"output":"DP-1","urgent":false,"window_count":0}
        ]}"#;

        let list = parse_event(line).unwrap();

        let action = focus_payload(list[0].id, &list).unwrap();

        assert_eq!(action["workspace"], json!("2"));
    }
}
