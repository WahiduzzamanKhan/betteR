//! `betterr --lsp-stdio -f kernel.json`: bridges an editor's stdio LSP client
//! to the LSP server of a running ark kernel. Lets the VS Code extension talk
//! to ark's live-session LSP without needing a ZeroMQ client of its own.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::jupyter::{ConnectionInfo, KernelClient};

pub fn run(info: &ConnectionInfo) -> Result<()> {
    let client = KernelClient::connect(info)?;
    client.wait_ready(Duration::from_secs(120), || None)?;
    let port = client.start_server_comm("lsp", Duration::from_secs(30), |_| {})?;
    let tcp = TcpStream::connect(("127.0.0.1", port)).context("connecting to ark LSP")?;

    let mut to_server = tcp.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(Shutdown::Write);
    });

    // Stdout is line-buffered and LSP bodies don't end in newlines, so flush
    // every chunk explicitly.
    let mut from_server = tcp;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = from_server.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
    // Keep the Jupyter connection (and its heartbeat) alive until the bridge ends.
    drop(client);
    Ok(())
}
