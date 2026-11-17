//! Startup output: what R prints while the session starts, mainly from
//! `.Rprofile` (e.g. renv's "Project loaded"). ark sends it on IOPub before any
//! command runs, so a console that attaches later (the VS Code extension
//! opens its console once the session is ready) would never see it.
//!
//! `better --kernel` therefore subscribes to its own IOPub before R starts and
//! records stream output, up to the first execution, in a file next to the
//! connection file. A console that attaches replays it.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::jupyter::{ConnectionInfo, IopubListener};

/// Enough for any realistic startup; stops a runaway profile filling the disk.
const MAX_BYTES: usize = 1 << 20;

/// `kernel.json` -> `kernel.startup.jsonl`.
pub fn path_for(connection_file: &Path) -> PathBuf {
    connection_file.with_extension("startup.jsonl")
}

/// Starts recording in the background. Must run before the kernel starts R.
pub fn record(info: &ConnectionInfo, connection_file: &Path) {
    let Ok(listener) = IopubListener::connect(info) else {
        return;
    };
    let path = path_for(connection_file);
    std::thread::spawn(move || {
        // Owner-only, like the connection file: it holds session output.
        let Ok(mut file) = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&path) else {
            return;
        };
        let mut written = 0;
        while let Ok(msg) = listener.recv() {
            match msg.msg_type() {
                // The first command: startup is over.
                "execute_input" => return,
                "stream" => {
                    let line = json!({ "name": msg.content["name"], "text": msg.content["text"] }).to_string() + "\n";
                    written += line.len();
                    if written > MAX_BYTES || file.write_all(line.as_bytes()).is_err() {
                        return;
                    }
                },
                _ => {},
            }
        }
    });
}

/// The recorded startup output as `(stream, text)` chunks, in order.
pub fn read(connection_file: &Path) -> Vec<(String, String)> {
    let Ok(file) = std::fs::File::open(path_for(connection_file)) else {
        return vec![];
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|v| Some((v["name"].as_str()?.to_string(), v["text"].as_str()?.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_sits_next_to_the_connection_file() {
        assert_eq!(path_for(Path::new("/tmp/x/kernel.json")), PathBuf::from("/tmp/x/kernel.startup.jsonl"));
    }

    #[test]
    fn reads_recorded_chunks_in_order() {
        let dir = std::env::temp_dir().join(format!("better-startup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = dir.join("kernel.json");
        std::fs::write(
            path_for(&conn),
            "{\"name\":\"stdout\",\"text\":\"a\\n\"}\nnot json\n{\"name\":\"stderr\",\"text\":\"b\\n\"}\n",
        )
        .unwrap();
        assert_eq!(read(&conn), vec![("stdout".into(), "a\n".into()), ("stderr".into(), "b\n".into())]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read(&conn).is_empty());
    }
}
