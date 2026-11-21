//! Finding why the kernel stopped, in ark's log, so the console can say so
//! instead of only pointing at the file (for example "Unsupported R version
//! 4.1.2. Ark requires R >= 4.2.0").
//!
//! The VS Code extension does the same in `src/arkLog.ts`; keep the two alike.

/// Longest reason shown; longer ones are cut with an ellipsis.
const MAX_CHARS: usize = 400;

/// The reason in ark's log `log`: the message of its first panic, else of its
/// last `ERROR` line, on one line.
///
/// A panic looks like `<time> ERROR  Panic! In file '…' at line N: <message>`,
/// and the message can go on over the following lines until a blank line or a
/// backtrace. When R code failed, the message quotes all of that code
/// (`Error evaluating '<code>': <R error>`); the code is left out.
pub fn failure_reason(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let reason = match lines.iter().position(|l| l.contains("Panic! In file '")) {
        Some(start) => {
            let first = lines[start];
            let first = match first.find("' at line ").and_then(|i| first[i..].find(": ").map(|j| i + j + 2)) {
                Some(i) => &first[i..],
                None => first,
            };
            let mut parts = vec![first];
            for line in &lines[start + 1..] {
                let t = line.trim();
                if t.is_empty() || t == "Backtrace:" || t == "R backtrace:" {
                    break;
                }
                parts.push(line);
            }
            parts.join("\n")
        },
        None => {
            let line = lines.iter().rev().find(|l| l.contains(" ERROR "))?;
            line[line.find(" ERROR ").unwrap() + " ERROR ".len()..].to_string()
        },
    };
    let mut reason = reason;
    if let Some(start) = reason.find("Error evaluating '")
        && let Some(end) = reason.rfind("': ")
        && end > start
    {
        reason.replace_range(start..end + 3, "");
    }
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        return None;
    }
    Some(match reason.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => format!("{}…", &reason[..i]),
        None => reason,
    })
}

/// [`failure_reason`] of the log file at `path`, if it can be read.
pub fn failure_reason_in(path: &std::path::Path) -> Option<String> {
    failure_reason(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::failure_reason;

    // Trimmed from real logs of `better` 0.2.0-rc.1 (2026-11-21).
    const OLD_R: &str = "  2026-11-21T09:49:01.416332Z  INFO  Loaded connection information from frontend in /tmp/better-6269/kernel.json
    at /github/home/.cargo/git/checkouts/ark/crates/amalthea/src/kernel.rs:358

  2026-11-21T09:49:01.429974Z ERROR  Panic! In file '/github/home/.cargo/git/checkouts/ark/crates/ark/src/start.rs' at line 55: Unsupported R version 4.1.2. Ark requires R >= 4.2.0
Backtrace:
   0: <std::backtrace::Backtrace>::create
   7: ark::start::start_kernel

    at /github/home/.cargo/git/checkouts/ark/crates/ark/src/panic.rs:52
";

    const NO_WHICH: &str = "  2026-11-21T09:51:39.635833Z ERROR  Panic! In file '/github/home/.cargo/git/checkouts/ark/crates/ark/src/console/console_repl.rs' at line 499: Failed to load R modules: Error evaluating 'import_positron(exprs = structure(expression(env_bind_force <- function(env,
    name, value) {
    invisible(original)
}), srcfile = <environment>, wholeSrcref = structure(c(1L, 0L,
25L, 0L, 0L, 0L, 1L, 25L), srcfile = <environment>, class = \"srcref\")))': .onLoad failed in loadNamespace() for 'utils', details:
  call: system(paste(which, shQuote(names[i])), intern = TRUE, ignore.stderr = TRUE)
  error: error in running command: 'Function not implemented'

R backtrace:
0. runHook(\".onLoad\", env, package.lib, package)
";

    #[test]
    fn a_panic_gives_its_message() {
        assert_eq!(
            failure_reason(OLD_R).as_deref(),
            Some("Unsupported R version 4.1.2. Ark requires R >= 4.2.0")
        );
        let no_r = "  2026-11-21T09:49:09.496557Z ERROR  Panic! In file '/x/crates/ark/src/start.rs' at line 46: Can't set up `R_HOME`: Can't find R or `R_HOME`: No such file or directory (os error 2)\nBacktrace:\n";
        assert_eq!(
            failure_reason(no_r).as_deref(),
            Some("Can't set up `R_HOME`: Can't find R or `R_HOME`: No such file or directory (os error 2)")
        );
    }

    #[test]
    fn quoted_r_code_is_left_out_of_a_multi_line_message() {
        assert_eq!(
            failure_reason(NO_WHICH).as_deref(),
            Some(
                "Failed to load R modules: .onLoad failed in loadNamespace() for 'utils', details: \
                 call: system(paste(which, shQuote(names[i])), intern = TRUE, ignore.stderr = TRUE) \
                 error: error in running command: 'Function not implemented'"
            )
        );
    }

    #[test]
    fn without_a_panic_the_last_error_line_is_used() {
        let log = "  t  INFO  starting\n  t ERROR  first problem\n  t ERROR  Can't connect\n  t  INFO  bye\n";
        assert_eq!(failure_reason(log).as_deref(), Some("Can't connect"));
        assert_eq!(failure_reason("  t  INFO  all good\n"), None);
        assert_eq!(failure_reason(""), None);
    }

    #[test]
    fn long_reasons_are_cut() {
        let log = format!("t ERROR  Panic! In file 'x' at line 1: {}\n", "é".repeat(1000));
        let reason = failure_reason(&log).unwrap();
        assert_eq!(reason.chars().count(), 401);
        assert!(reason.ends_with('…'));
    }
}
