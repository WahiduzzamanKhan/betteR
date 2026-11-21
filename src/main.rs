//! betteR console: a terminal R console that drives an `ark` kernel (built in) over
//! Jupyter, with tree-sitter highlighting and LSP-backed completion.

mod arklog;
mod assist;
mod help_box;
mod highlight;
mod ide;
mod jupyter;
mod kernel;
mod kernel_mode;
mod lsp;
mod plots;
mod proxy;
mod repl;
mod startup;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::jupyter::{Channel, ConnectionInfo, KernelClient};
use crate::repl::{Origin, Repl};

const USAGE: &str = "\
better — modern R console backed by ark

USAGE:
    better [OPTIONS]

OPTIONS:
    -f, --connection-file <PATH>  Attach to a running kernel instead of starting one
        --ark <PATH>              Use a separate ark binary instead of the built-in kernel
        --log <PATH>              Write the launched kernel's log here
        --no-lsp                  Disable LSP-backed completion
        --lsp-stdio               Bridge ark's LSP to stdin/stdout for an editor (needs -f)
        --ide <PORT>              Connect to a host IDE on 127.0.0.1:<PORT> (Run Selection, debugger,
                                  completion); its token must be in BETTER_IDE_TOKEN
        --kernel                  Run as the R kernel (used internally; needs --connection-file)
        --shutdown                End the session of the kernel at --connection-file, as q() would
    -h, --help                    Show this help
    -V, --version                 Show version

Piped stdin (e.g. `echo 'summary(mtcars)' | better`) runs the input and exits.";

struct Args {
    connection_file: Option<PathBuf>,
    ark: Option<PathBuf>,
    log: Option<PathBuf>,
    lsp: bool,
    lsp_stdio: bool,
    shutdown: bool,
    ide_port: Option<u16>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        connection_file: None,
        ark: None,
        log: None,
        lsp: true,
        lsp_stdio: false,
        shutdown: false,
        ide_port: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-f" | "--connection-file" => args.connection_file = Some(value(&arg)?.into()),
            "--ark" => args.ark = Some(value(&arg)?.into()),
            "--log" => args.log = Some(value(&arg)?.into()),
            "--no-lsp" => args.lsp = false,
            "--lsp-stdio" => args.lsp_stdio = true,
            "--shutdown" => args.shutdown = true,
            "--ide" => {
                let port = value(&arg)?;
                args.ide_port = Some(port.parse().with_context(|| format!("--ide needs a port number, not `{port}`"))?);
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            },
            "-V" | "--version" => {
                println!("better {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            },
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

/// Asks the kernel to end its session. Ark interrupts any running code, then
/// lets R quit as at the end of its input: R runs `.Last` and exit finalizers
/// and removes its temp directory, unlike when its process is killed (on
/// Windows, Node's `kill()` always kills outright). Returns once ark has
/// accepted; the kernel process exits shortly after.
fn request_shutdown(client: &KernelClient, timeout: Duration) -> Result<()> {
    let id = client.send(Channel::Control, "shutdown_request", json!({ "restart": false }))?;
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Some((_, msg)) = client.poll(&[Channel::Control], left)? else {
            continue;
        };
        if msg.msg_type() == "shutdown_reply" && msg.parent_id() == Some(id.as_str()) {
            if msg.content["status"] != "ok" {
                bail!("the kernel refused to shut down: {}", msg.content);
            }
            return Ok(());
        }
    }
    bail!("the kernel didn't answer the shutdown request within {timeout:?}")
}

fn main() {
    if kernel_mode::requested() {
        if let Err(err) = kernel_mode::run() {
            eprintln!("better --kernel: {err:#}");
            std::process::exit(1);
        }
        return;
    }
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("better: {err:#}");
            std::process::exit(1);
        },
    }
}

fn run() -> Result<i32> {
    let Some(args) = parse_args()? else {
        return Ok(0);
    };

    if args.lsp_stdio {
        let path = args.connection_file.as_ref().context("--lsp-stdio needs --connection-file")?;
        proxy::run(&ConnectionInfo::read(path)?)?;
        return Ok(0);
    }

    if args.shutdown {
        let path = args.connection_file.as_ref().context("--shutdown needs --connection-file")?;
        let client = KernelClient::connect(&ConnectionInfo::read(path)?)?;
        request_shutdown(&client, Duration::from_secs(5))?;
        return Ok(0);
    }

    // Ctrl-C outside of line editing (i.e. while R is busy) interrupts the
    // kernel rather than killing the console.
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    }

    let (info, origin) = match &args.connection_file {
        Some(path) => (ConnectionInfo::read(path)?, Origin::Attached(path.clone())),
        None => {
            let program = kernel::KernelProgram::from_flag(args.ark.clone())?;
            let (kernel, info) = kernel::spawn(&program, args.log.clone())?;
            (info, Origin::Spawned(kernel))
        },
    };
    // Checked before connecting, so a missing token fails fast and clearly.
    let ide = match args.ide_port {
        Some(port) => {
            let token = std::env::var("BETTER_IDE_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .context("--ide needs the IDE's token in the BETTER_IDE_TOKEN environment variable")?;
            Some((port, token))
        },
        None => None,
    };
    let client = KernelClient::connect(&info)?;
    let mut repl = Repl::start(client, origin, args.lsp, interrupted, ide)?;

    if std::io::stdin().is_terminal() {
        repl.run()?;
        Ok(0)
    } else {
        let mut code = String::new();
        std::io::stdin().read_to_string(&mut code)?;
        Ok(if repl.run_script(&code)? { 0 } else { 1 })
    }
}
