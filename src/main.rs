//! betterR console: a terminal R console that drives an `ark` kernel over
//! Jupyter, with tree-sitter highlighting and LSP-backed completion.

mod highlight;
mod jupyter;
mod kernel;
mod lsp;
mod proxy;
mod repl;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::jupyter::{ConnectionInfo, KernelClient};
use crate::repl::{Origin, Repl};

const USAGE: &str = "\
betterr — modern R console backed by ark

USAGE:
    betterr [OPTIONS]

OPTIONS:
    -f, --connection-file <PATH>  Attach to a running kernel instead of starting one
        --ark <PATH>              ark binary to launch (default: $ARK_PATH, PATH, dev build)
        --log <PATH>              Write the launched kernel's log here
        --no-lsp                  Disable LSP-backed completion
        --lsp-stdio               Bridge ark's LSP to stdin/stdout for an editor (needs -f)
    -h, --help                    Show this help
    -V, --version                 Show version

Piped stdin (e.g. `echo 'summary(mtcars)' | betterr`) runs the input and exits.";

struct Args {
    connection_file: Option<PathBuf>,
    ark: Option<PathBuf>,
    log: Option<PathBuf>,
    lsp: bool,
    lsp_stdio: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        connection_file: None,
        ark: None,
        log: None,
        lsp: true,
        lsp_stdio: false,
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
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            },
            "-V" | "--version" => {
                println!("betterr {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            },
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("betterr: {err:#}");
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

    // Ctrl-C outside of line editing (i.e. while R is busy) interrupts the
    // kernel rather than killing the console.
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    }

    let (info, origin) = match &args.connection_file {
        Some(path) => (ConnectionInfo::read(path)?, Origin::Attached),
        None => {
            let ark = kernel::find_ark(args.ark.clone())?;
            let (kernel, info) = kernel::spawn(&ark, args.log.clone())?;
            (info, Origin::Spawned(kernel))
        },
    };
    let client = KernelClient::connect(&info)?;
    let mut repl = Repl::start(client, origin, args.lsp, interrupted)?;

    if std::io::stdin().is_terminal() {
        repl.run()?;
        Ok(0)
    } else {
        let mut code = String::new();
        std::io::stdin().read_to_string(&mut code)?;
        Ok(if repl.run_script(&code)? { 0 } else { 1 })
    }
}
