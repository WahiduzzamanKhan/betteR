//! `better --kernel`: runs ark, built into this binary, as the session's R
//! kernel. The console (and the VS Code extension) start the session by
//! launching this same executable in kernel mode, so `better` is the only
//! program needed besides R itself. Mirrors ark's own `main`.

use anyhow::{bail, Context, Result};
use ark::console::SessionMode;
use ark::repos::DefaultRepos;

/// True if this process was started in kernel mode.
pub fn requested() -> bool {
    std::env::args().nth(1).as_deref() == Some("--kernel")
}

/// Never returns on success: ark runs R's event loop until the session ends.
pub fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    ensure_r_lib_on_loader_path()?;

    // Both must happen on the main thread before any other thread exists.
    ark::panic::mark_r_thread();
    ark::signals::initialize_signal_block();

    let mut connection_file = None;
    let mut log_file = None;
    let mut session_mode = SessionMode::Console;
    let mut record_startup = true;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            // ark's own spelling is accepted too, so callers can treat both alike.
            "--connection-file" | "--connection_file" | "-f" => connection_file = Some(value()?),
            "--log" => log_file = Some(value()?),
            // From a console that starts its own kernel; see `startup`.
            "--no-startup-record" => record_startup = false,
            "--session-mode" => {
                session_mode = match value()?.as_str() {
                    "console" => SessionMode::Console,
                    "notebook" => SessionMode::Notebook,
                    "background" => SessionMode::Background,
                    other => bail!("unknown session mode `{other}`"),
                }
            },
            other => bail!("unknown kernel argument `{other}`"),
        }
    }
    let connection_file = connection_file.context("--kernel needs --connection-file")?;

    ark::logger::init(log_file.as_deref(), None);
    ark::traps::register_trap_handlers();
    ark::panic::install();

    let (connection, registration) = amalthea::kernel::read_connection(&connection_file);
    // Before R starts, so `.Rprofile` output is caught for consoles that attach later.
    if record_startup {
        if let Ok(info) = crate::jupyter::ConnectionInfo::read(std::path::Path::new(&connection_file)) {
            crate::startup::record(&info, std::path::Path::new(&connection_file));
        }
    }
    // Same R arguments as ark: no workspace save/restore prompts.
    let r_args = vec!["--no-save".into(), "--no-restore-data".into(), "--interactive".into()];
    ark::start::start_kernel(connection, registration, r_args, None, session_mode, true, DefaultRepos::Auto);
    Ok(())
}

/// R's package libraries (e.g. `utils.so`) link to `libR.so` by name, and on
/// Linux the loader finds it only via `LD_LIBRARY_PATH`, which R's own launcher
/// sets and ark expects its caller to set. The loader reads it at process
/// start, so if `$R_HOME/lib` is missing, add it and restart this process.
#[cfg(target_os = "linux")]
fn ensure_r_lib_on_loader_path() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;

    let r_home = match std::env::var_os("R_HOME").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home),
        None => match std::process::Command::new("R").arg("RHOME").output() {
            Ok(out) if out.status.success() => PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
            // ark reports a missing R itself.
            _ => return Ok(()),
        },
    };
    let lib = r_home.join("lib");
    let current = std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).filter(|p| !p.as_os_str().is_empty()).collect();
    if paths.contains(&lib) {
        return Ok(());
    }
    paths.insert(0, lib);
    let err = std::process::Command::new(std::env::current_exe().context("locating the better binary")?)
        .args(std::env::args_os().skip(1))
        .env("LD_LIBRARY_PATH", std::env::join_paths(paths)?)
        .env("R_HOME", &r_home)
        .exec();
    Err(err).context("restarting the kernel with R's library path")
}
