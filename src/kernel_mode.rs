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
    // Both must happen on the main thread before any other thread exists.
    ark::panic::mark_r_thread();
    ark::signals::initialize_signal_block();

    let mut connection_file = None;
    let mut log_file = None;
    let mut session_mode = SessionMode::Console;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            // ark's own spelling is accepted too, so callers can treat both alike.
            "--connection-file" | "--connection_file" | "-f" => connection_file = Some(value()?),
            "--log" => log_file = Some(value()?),
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
    // Same R arguments as ark: no workspace save/restore prompts.
    let r_args = vec!["--no-save".into(), "--no-restore-data".into(), "--interactive".into()];
    ark::start::start_kernel(connection, registration, r_args, None, session_mode, true, DefaultRepos::Auto);
    Ok(())
}
