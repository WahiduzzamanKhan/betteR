//! Settings: built-in defaults, overridden in turn by `config.toml`, by
//! environment variables and by command-line flags (`--set key=value`,
//! `--no-lsp`, `--ark`). Each value remembers where it came from, for
//! `--print-config`.
//!
//! A bad entry (unknown key, wrong type, out of range) never stops the
//! console: it becomes a warning and the key keeps the value from below.
//! Read once at startup; the console only writes the file to create the
//! template or reset it.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nu_ansi_term::Color;

/// Written on first start and by `--reset-config`. Every key is commented
/// out, so the file changes nothing until a line is uncommented, and a later
/// release that changes a default still applies.
pub const TEMPLATE: &str = r#"[startup]
# r_home = ""
# ark = ""
# banner = true
# inline_plots = "auto"   # auto | iterm | kitty | sixel | none

[completion]
# lsp = true
# popup = "typing"        # typing | tab
# min_chars = 1           # only with popup = "typing"
# delay_ms = 0            # only with popup = "typing"
# accept = "tab"          # tab | enter
# max_height = 5

[editing]
# shift_enter = "auto"    # auto | on | off

[history]
# size = 10000
# suggestions = true
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Path,
    Bool,
    Int { min: i64, max: i64 },
    Choice(&'static [&'static str]),
}

struct Key {
    name: &'static str,
    kind: Kind,
    default: Value,
}

/// Every setting, in the order of the template and `--print-config`.
static KEYS: &[Key] = &[
    Key { name: "startup.r_home", kind: Kind::Path, default: Value::Text(String::new()) },
    Key { name: "startup.ark", kind: Kind::Path, default: Value::Text(String::new()) },
    Key { name: "startup.banner", kind: Kind::Bool, default: Value::Bool(true) },
    Key {
        name: "startup.inline_plots",
        kind: Kind::Choice(&["auto", "iterm", "kitty", "sixel", "none"]),
        default: Value::Text(String::new()),
    },
    Key { name: "completion.lsp", kind: Kind::Bool, default: Value::Bool(true) },
    Key { name: "completion.popup", kind: Kind::Choice(&["typing", "tab"]), default: Value::Text(String::new()) },
    Key { name: "completion.min_chars", kind: Kind::Int { min: 0, max: 10 }, default: Value::Int(1) },
    Key { name: "completion.delay_ms", kind: Kind::Int { min: 0, max: 2000 }, default: Value::Int(0) },
    Key { name: "completion.accept", kind: Kind::Choice(&["tab", "enter"]), default: Value::Text(String::new()) },
    Key { name: "completion.max_height", kind: Kind::Int { min: 1, max: 50 }, default: Value::Int(5) },
    Key {
        name: "editing.shift_enter",
        kind: Kind::Choice(&["auto", "on", "off"]),
        default: Value::Text(String::new()),
    },
    Key { name: "history.size", kind: Kind::Int { min: 0, max: i64::MAX }, default: Value::Int(10_000) },
    Key { name: "history.suggestions", kind: Kind::Bool, default: Value::Bool(true) },
];

impl Key {
    /// A choice's default is its first value (a static can't build a `String`
    /// with text in it).
    fn default_value(&self) -> Value {
        match (self.kind, &self.default) {
            (Kind::Choice(values), _) => Value::Text(values[0].to_string()),
            (_, value) => value.clone(),
        }
    }

    /// A value from the config file.
    fn parse_toml(&self, value: &toml::Value) -> Result<Value, String> {
        let value = match (self.kind, value) {
            (Kind::Path | Kind::Choice(_), toml::Value::String(s)) => Value::Text(s.clone()),
            (Kind::Bool, toml::Value::Boolean(b)) => Value::Bool(*b),
            (Kind::Int { .. }, toml::Value::Integer(i)) => Value::Int(*i),
            _ => return Err(format!("must be {}, not {}", self.expected(), toml_type(value))),
        };
        self.check(value)
    }

    /// A value from the command line or the environment, as typed.
    fn parse_text(&self, text: &str) -> Result<Value, String> {
        let value = match self.kind {
            Kind::Path | Kind::Choice(_) => Value::Text(unquote(text).to_string()),
            Kind::Bool => match text {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(format!("must be {}, not `{text}`", self.expected())),
            },
            Kind::Int { .. } => match text.parse() {
                Ok(i) => Value::Int(i),
                Err(_) => return Err(format!("must be {}, not `{text}`", self.expected())),
            },
        };
        self.check(value)
    }

    fn check(&self, value: Value) -> Result<Value, String> {
        match (self.kind, &value) {
            (Kind::Int { min, max }, Value::Int(i)) if i < &min || i > &max => {
                Err(format!("must be {}, not {i}", self.expected()))
            },
            (Kind::Choice(values), Value::Text(s)) if !values.contains(&s.as_str()) => {
                Err(format!("must be {}, not \"{s}\"", self.expected()))
            },
            _ => Ok(value),
        }
    }

    fn expected(&self) -> String {
        match self.kind {
            Kind::Path => "a path in quotes".into(),
            Kind::Bool => "true or false".into(),
            Kind::Int { min, max: i64::MAX } => format!("a whole number, {min} or more"),
            Kind::Int { min, max } => format!("a whole number from {min} to {max}"),
            Kind::Choice(values) => {
                let quoted: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
                format!("one of {}", quoted.join(", "))
            },
        }
    }
}

fn toml_type(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "text",
        toml::Value::Integer(_) => "a whole number",
        toml::Value::Float(_) => "a decimal number",
        toml::Value::Boolean(_) => "true/false",
        toml::Value::Datetime(_) => "a date",
        toml::Value::Array(_) => "a list",
        toml::Value::Table(_) => "a table",
    }
}

/// `--set startup.r_home="C:\R"` reaches us with its quotes when a shell
/// doesn't remove them (cmd.exe, a VS Code argument list).
fn unquote(text: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(q).and_then(|t| t.strip_suffix(q)) {
            return inner;
        }
    }
    text
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Text(String),
    Bool(bool),
    Int(i64),
}

impl fmt::Display for Value {
    /// As TOML writes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Text(s) => write!(f, "{}", toml::Value::String(s.clone())),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
        }
    }
}

/// Where a setting's value came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Default,
    File,
    Env(&'static str),
    /// `--set`, `--no-lsp` or `--ark`.
    Flag(&'static str),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Default => write!(f, "default"),
            Source::File => write!(f, "file"),
            Source::Env(var) => write!(f, "env {var}"),
            Source::Flag(flag) => write!(f, "flag {flag}"),
        }
    }
}

/// A setting given on the command line, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub struct Flag {
    pub flag: &'static str,
    pub key: String,
    pub value: String,
}

impl Flag {
    /// `--set key=value`.
    pub fn set(arg: &str) -> Self {
        match arg.split_once('=') {
            Some((key, value)) => Flag { flag: "--set", key: key.trim().to_string(), value: value.to_string() },
            // Reported, with what was given, when the settings are applied.
            None => Flag { flag: "--set", key: String::new(), value: arg.to_string() },
        }
    }
}

/// The settings `--config`, `--set` and the flags they stand in for; shared
/// by the console's and the kernel's argument parsing.
#[derive(Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub flags: Vec<Flag>,
}

impl Args {
    /// Takes `--config <path>` or `--set <key=value>`; false for any other argument.
    pub fn take(&mut self, arg: &str, value: &mut dyn FnMut() -> Result<String>) -> Result<bool> {
        match arg {
            "--config" => self.config = Some(value()?.into()),
            "--set" => {
                let v = value()?;
                self.flags.push(Flag::set(&v));
            },
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InlinePlots {
    Auto,
    Iterm,
    Kitty,
    Sixel,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Popup {
    Typing,
    Tab,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Accept {
    Tab,
    Enter,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShiftEnter {
    Auto,
    On,
    Off,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The config file, whether or not it exists.
    pub path: Option<PathBuf>,
    values: Vec<(Value, Source)>,
    /// Bad entries and deprecated variables, one line each.
    pub warnings: Vec<String>,
}

impl Config {
    /// The settings from `path`, the process environment and `flags`.
    pub fn load(path: Option<PathBuf>, flags: &[Flag]) -> Self {
        let text = path.as_deref().and_then(|p| match std::fs::read_to_string(p) {
            Ok(text) => Some(Ok(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => Some(Err(err.to_string())),
        });
        Self::from_parts(path, text, &|var| std::env::var(var).ok(), flags)
    }

    /// The layers, lowest first; `env` reads one environment variable.
    fn from_parts(
        path: Option<PathBuf>,
        file: Option<Result<String, String>>,
        env: &dyn Fn(&str) -> Option<String>,
        flags: &[Flag],
    ) -> Self {
        let mut config = Config {
            path,
            values: KEYS.iter().map(|k| (k.default_value(), Source::Default)).collect(),
            warnings: Vec::new(),
        };
        let where_ = config.path.as_ref().map_or("config file".into(), |p| p.display().to_string());
        match file {
            Some(Ok(text)) => config.apply_file(&text, &where_),
            Some(Err(err)) => config.warn(format!("{where_}: can't read it ({err}); using the defaults")),
            None => {},
        }
        config.apply_env(env);
        for flag in flags {
            config.apply_flag(flag);
        }
        config
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    fn index(name: &str) -> Option<usize> {
        KEYS.iter().position(|k| k.name == name)
    }

    fn apply_file(&mut self, text: &str, where_: &str) {
        let table = match text.parse::<toml::Table>() {
            Ok(table) => table,
            Err(err) => {
                let message = err.message().trim().to_string();
                let line = err.span().map(|s| text[..s.start].matches('\n').count() + 1);
                let at = line.map_or(String::new(), |l| format!(" (line {l})"));
                self.warn(format!("{where_}{at}: {message}; using the defaults"));
                return;
            },
        };
        for (section, entries) in &table {
            let Some(entries) = entries.as_table() else {
                self.warn(format!("{where_}: unknown setting `{section}`"));
                continue;
            };
            for (key, value) in entries {
                let name = format!("{section}.{key}");
                let Some(i) = Self::index(&name) else {
                    self.warn(format!("{where_}: unknown setting `{name}`"));
                    continue;
                };
                match KEYS[i].parse_toml(value) {
                    Ok(v) => self.values[i] = (v, Source::File),
                    Err(problem) => {
                        let default = KEYS[i].default_value();
                        self.warn(format!("{where_}: `{name}` {problem}; using the default ({default})"));
                    },
                }
            }
        }
    }

    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let get = |var| env(var).filter(|v| !v.is_empty());
        // R's own variable, not deprecated.
        if let Some(home) = get("R_HOME") {
            self.put("startup.r_home", Value::Text(home), Source::Env("R_HOME"));
        }
        if let Some(value) = get("BETTER_INLINE_IMAGES") {
            self.deprecated("BETTER_INLINE_IMAGES", "startup.inline_plots", &value, &value);
        }
        if let Some(value) = get("BETTER_SHIFT_ENTER") {
            let mapped = match value.as_str() {
                "1" => "on",
                "0" => "off",
                other => other,
            };
            self.deprecated("BETTER_SHIFT_ENTER", "editing.shift_enter", &value, mapped);
        }
    }

    /// A variable that a setting replaces: still honoured, with a warning.
    fn deprecated(&mut self, var: &'static str, name: &str, raw: &str, value: &str) {
        self.warn(format!("{var} is deprecated: use the setting `{name}` instead"));
        let i = Self::index(name).expect("known setting");
        match KEYS[i].parse_text(value) {
            Ok(v) => self.values[i] = (v, Source::Env(var)),
            Err(_) => self.warn(format!("{var}=`{raw}` isn't understood; ignored")),
        }
    }

    fn apply_flag(&mut self, flag: &Flag) {
        if flag.key.is_empty() {
            self.warn(format!("{} `{}`: expected key=value; ignored", flag.flag, flag.value));
            return;
        }
        let Some(i) = Self::index(&flag.key) else {
            self.warn(format!("{}: unknown setting `{}`; ignored", flag.flag, flag.key));
            return;
        };
        match KEYS[i].parse_text(&flag.value) {
            Ok(v) => self.values[i] = (v, Source::Flag(flag.flag)),
            Err(problem) => self.warn(format!("{}: `{}` {problem}; ignored", flag.flag, flag.key)),
        }
    }

    fn put(&mut self, name: &str, value: Value, source: Source) {
        let i = Self::index(name).expect("known setting");
        self.values[i] = (value, source);
    }

    fn get(&self, name: &str) -> &Value {
        &self.values[Self::index(name).expect("known setting")].0
    }

    fn text(&self, name: &str) -> &str {
        match self.get(name) {
            Value::Text(s) => s,
            _ => unreachable!("{name} is text"),
        }
    }

    fn flag(&self, name: &str) -> bool {
        match self.get(name) {
            Value::Bool(b) => *b,
            _ => unreachable!("{name} is true/false"),
        }
    }

    fn int(&self, name: &str) -> i64 {
        match self.get(name) {
            Value::Int(i) => *i,
            _ => unreachable!("{name} is a number"),
        }
    }

    #[cfg(test)]
    fn source(&self, name: &str) -> &Source {
        &self.values[Self::index(name).expect("known setting")].1
    }

    /// Empty: `$R_HOME`, else `R RHOME`.
    pub fn r_home(&self) -> Option<&str> {
        Some(self.text("startup.r_home")).filter(|s| !s.is_empty())
    }

    /// Empty: the built-in kernel.
    pub fn ark(&self) -> Option<PathBuf> {
        Some(self.text("startup.ark")).filter(|s| !s.is_empty()).map(PathBuf::from)
    }

    pub fn banner(&self) -> bool {
        self.flag("startup.banner")
    }

    pub fn inline_plots(&self) -> InlinePlots {
        match self.text("startup.inline_plots") {
            "iterm" => InlinePlots::Iterm,
            "kitty" => InlinePlots::Kitty,
            "sixel" => InlinePlots::Sixel,
            "none" => InlinePlots::None,
            _ => InlinePlots::Auto,
        }
    }

    pub fn lsp(&self) -> bool {
        self.flag("completion.lsp")
    }

    pub fn popup(&self) -> Popup {
        if self.text("completion.popup") == "tab" { Popup::Tab } else { Popup::Typing }
    }

    pub fn min_chars(&self) -> usize {
        self.int("completion.min_chars") as usize
    }

    pub fn delay_ms(&self) -> u64 {
        self.int("completion.delay_ms") as u64
    }

    pub fn accept(&self) -> Accept {
        if self.text("completion.accept") == "enter" { Accept::Enter } else { Accept::Tab }
    }

    pub fn max_height(&self) -> u16 {
        self.int("completion.max_height") as u16
    }

    pub fn shift_enter(&self) -> ShiftEnter {
        match self.text("editing.shift_enter") {
            "on" => ShiftEnter::On,
            "off" => ShiftEnter::Off,
            _ => ShiftEnter::Auto,
        }
    }

    pub fn history_size(&self) -> usize {
        self.int("history.size") as usize
    }

    pub fn history_suggestions(&self) -> bool {
        self.flag("history.suggestions")
    }

    /// Prints the warnings, dimmed, one line each.
    pub fn print_warnings(&self) {
        let dim = Color::DarkGray.normal();
        for warning in &self.warnings {
            eprintln!("{}", dim.paint(format!("better: {warning}")));
        }
    }

    /// `--print-config`: every setting, its value and where it came from.
    pub fn describe(&self) -> String {
        let mut out = match &self.path {
            Some(p) if p.is_file() => format!("# {}\n", p.display()),
            Some(p) => format!("# {} (not found)\n", p.display()),
            None => "# no config file location\n".into(),
        };
        let lines: Vec<(String, &Source)> =
            KEYS.iter().zip(&self.values).map(|(k, (v, s))| (format!("{} = {v}", k.name), s)).collect();
        let width = lines.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
        for (line, source) in lines {
            out.push_str(&format!("{line:width$}  # {source}\n"));
        }
        out
    }
}

/// Where the config file is: `--config`, else `$BETTER_CONFIG`, else
/// `$XDG_CONFIG_HOME/better/config.toml` or `~/.config/better/config.toml`
/// (also on macOS, where `dirs::config_dir()` is `~/Library/Application
/// Support`; the installer's receipt is in `~/.config/better` already), and
/// `%APPDATA%\better\config.toml` on Windows.
pub fn path(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| std::env::var_os("BETTER_CONFIG").filter(|p| !p.is_empty()).map(PathBuf::from))
        .or_else(default_path)
}

#[cfg(windows)]
fn default_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("better").join("config.toml"))
}

#[cfg(not(windows))]
fn default_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = xdg.or_else(|| Some(dirs::home_dir()?.join(".config")))?;
    Some(dir.join("better").join("config.toml"))
}

/// First start: writes the template if there's no file. Never overwrites one,
/// so upgrades keep the user's settings. Failing is fine (read-only home).
pub fn write_template_if_missing(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        let _ = file.write_all(TEMPLATE.as_bytes());
    }
}

/// `--reset-config`: replaces the file with the template, keeping the old one
/// as `config.toml.bak`. Asks first unless `yes`; returns false if declined.
pub fn reset(path: &Path, yes: bool) -> Result<bool> {
    if !yes {
        print!("Reset {} to defaults? [y/N] ", path.display());
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            println!("Nothing changed.");
            return Ok(false);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.exists() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        let backup = PathBuf::from(backup);
        std::fs::copy(path, &backup).with_context(|| format!("saving {}", backup.display()))?;
        std::fs::write(path, TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
        println!("Wrote {}\nThe old file is saved as {}", path.display(), backup.display());
    } else {
        std::fs::write(path, TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
        println!("Wrote {}", path.display());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(file: Option<&str>, env: &[(&str, &str)], flags: &[Flag]) -> Config {
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_parts(
            Some(PathBuf::from("/cfg/config.toml")),
            file.map(|f| Ok(f.to_string())),
            &move |var| env.iter().find(|(k, _)| k == var).map(|(_, v)| v.clone()),
            flags,
        )
    }

    fn set(arg: &str) -> Flag {
        Flag::set(arg)
    }

    #[test]
    fn defaults() {
        let c = load(None, &[], &[]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.r_home(), None);
        assert_eq!(c.ark(), None);
        assert!(c.banner());
        assert_eq!(c.inline_plots(), InlinePlots::Auto);
        assert!(c.lsp());
        assert_eq!(c.popup(), Popup::Typing);
        assert_eq!(c.min_chars(), 1);
        assert_eq!(c.delay_ms(), 0);
        assert_eq!(c.accept(), Accept::Tab);
        assert_eq!(c.max_height(), 5);
        assert_eq!(c.shift_enter(), ShiftEnter::Auto);
        assert_eq!(c.history_size(), 10_000);
        assert!(c.history_suggestions());
        assert!(KEYS.iter().all(|k| *c.source(k.name) == Source::Default));
    }

    #[test]
    fn parses_each_key_from_the_file() {
        let c = load(
            Some(
                r#"
                [startup]
                r_home = "/opt/R"
                ark = "/opt/ark"
                banner = false
                inline_plots = "sixel"
                [completion]
                lsp = false
                popup = "tab"
                min_chars = 3
                delay_ms = 300
                accept = "enter"
                max_height = 12
                [editing]
                shift_enter = "off"
                [history]
                size = 0
                suggestions = false
                "#,
            ),
            &[],
            &[],
        );
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.r_home(), Some("/opt/R"));
        assert_eq!(c.ark(), Some(PathBuf::from("/opt/ark")));
        assert!(!c.banner());
        assert_eq!(c.inline_plots(), InlinePlots::Sixel);
        assert!(!c.lsp());
        assert_eq!(c.popup(), Popup::Tab);
        assert_eq!(c.min_chars(), 3);
        assert_eq!(c.delay_ms(), 300);
        assert_eq!(c.accept(), Accept::Enter);
        assert_eq!(c.max_height(), 12);
        assert_eq!(c.shift_enter(), ShiftEnter::Off);
        assert_eq!(c.history_size(), 0);
        assert!(!c.history_suggestions());
        assert!(KEYS.iter().all(|k| *c.source(k.name) == Source::File));
    }

    #[test]
    fn each_level_overrides_the_one_below() {
        let file = "[startup]\nr_home = \"/file\"\ninline_plots = \"kitty\"\n[completion]\nmin_chars = 2\nlsp = true\n";
        let c = load(Some(file), &[], &[]);
        assert_eq!((c.r_home(), c.source("startup.r_home")), (Some("/file"), &Source::File));

        let c = load(Some(file), &[("R_HOME", "/env"), ("BETTER_INLINE_IMAGES", "iterm")], &[]);
        assert_eq!((c.r_home(), c.source("startup.r_home")), (Some("/env"), &Source::Env("R_HOME")));
        assert_eq!(c.inline_plots(), InlinePlots::Iterm);
        assert_eq!(c.min_chars(), 2);

        let flags = [
            set("startup.r_home=/flag"),
            set("startup.inline_plots=none"),
            set("completion.min_chars=4"),
            Flag { flag: "--no-lsp", key: "completion.lsp".into(), value: "false".into() },
        ];
        let c = load(Some(file), &[("R_HOME", "/env"), ("BETTER_INLINE_IMAGES", "iterm")], &flags);
        assert_eq!((c.r_home(), c.source("startup.r_home")), (Some("/flag"), &Source::Flag("--set")));
        assert_eq!(c.inline_plots(), InlinePlots::None);
        assert_eq!(c.min_chars(), 4);
        assert_eq!((c.lsp(), c.source("completion.lsp")), (false, &Source::Flag("--no-lsp")));
        // The last flag wins.
        let c = load(None, &[], &[set("completion.accept=enter"), set("completion.accept=tab")]);
        assert_eq!(c.accept(), Accept::Tab);
    }

    #[test]
    fn bad_entries_warn_and_keep_the_default() {
        let c = load(
            Some(
                "[startup]\nbanner = \"no\"\ncolour = 1\n[completion]\nmin_chars = 20\naccept = \"space\"\n\
                 max_height = 5\n[theme]\nx = 1\n",
            ),
            &[],
            &[],
        );
        assert!(c.banner());
        assert_eq!(c.min_chars(), 1);
        assert_eq!(c.accept(), Accept::Tab);
        // A good entry beside bad ones still counts.
        assert_eq!(*c.source("completion.max_height"), Source::File);
        let w = c.warnings.join("\n");
        assert_eq!(c.warnings.len(), 5, "{w}");
        for needle in [
            "/cfg/config.toml: `startup.banner` must be true or false, not text; using the default (true)",
            "/cfg/config.toml: unknown setting `startup.colour`",
            "`completion.min_chars` must be a whole number from 0 to 10, not 20; using the default (1)",
            "`completion.accept` must be one of \"tab\", \"enter\", not \"space\"; using the default (\"tab\")",
            "unknown setting `theme.x`",
        ] {
            assert!(w.contains(needle), "missing {needle:?} in\n{w}");
        }
    }

    #[test]
    fn a_broken_file_warns_once_and_uses_the_defaults() {
        let c = load(Some("[completion\nmin_chars = 3\n"), &[], &[]);
        assert_eq!(c.min_chars(), 1);
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings[0].starts_with("/cfg/config.toml (line 1): "), "{:?}", c.warnings);
    }

    #[test]
    fn a_missing_file_is_fine() {
        let c = Config::load(Some(std::env::temp_dir().join("better-no-such-dir/config.toml")), &[]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.min_chars(), 1);
    }

    #[test]
    fn set_parsing() {
        assert_eq!(set("completion.accept=enter"), Flag { flag: "--set", key: "completion.accept".into(), value: "enter".into() });
        // Only the first `=` splits; a Windows path keeps its `=`-free text as is.
        assert_eq!(set("startup.r_home=C:\\R=4").value, "C:\\R=4");
        let c = load(None, &[], &[set("startup.r_home=\"C:\\Program Files\\R\""), set("completion.delay_ms=250")]);
        assert_eq!(c.r_home(), Some("C:\\Program Files\\R"));
        assert_eq!(c.delay_ms(), 250);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);

        let c = load(
            None,
            &[],
            &[set("completion.min_chars"), set("nope.key=1"), set("startup.banner=yes"), set("history.size=-1")],
        );
        assert_eq!(c.warnings.len(), 4, "{:?}", c.warnings);
        assert!(c.warnings[0].contains("--set `completion.min_chars`: expected key=value"), "{:?}", c.warnings);
        assert!(c.warnings[1].contains("unknown setting `nope.key`"));
        assert!(c.warnings[2].contains("`startup.banner` must be true or false, not `yes`"));
        assert!(c.warnings[3].contains("`history.size` must be a whole number, 0 or more, not -1"));
        assert!(c.banner());
        assert_eq!(c.history_size(), 10_000);
    }

    #[test]
    fn template_lists_every_key_with_its_default() {
        let c = load(Some(TEMPLATE), &[], &[]);
        assert!(c.warnings.is_empty(), "the template must parse cleanly: {:?}", c.warnings);
        assert!(KEYS.iter().all(|k| *c.source(k.name) == Source::Default), "every key is commented out");
        let mut section = "";
        let mut listed = Vec::new();
        for line in TEMPLATE.lines() {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name;
            } else if let Some((key, rest)) = line.strip_prefix("# ").and_then(|l| l.split_once(" = ")) {
                let value = rest.split("  #").next().unwrap().trim();
                listed.push((format!("{section}.{key}"), value.to_string()));
            }
        }
        let expected: Vec<(String, String)> =
            KEYS.iter().map(|k| (k.name.to_string(), k.default_value().to_string())).collect();
        assert_eq!(listed, expected);
    }

    #[test]
    fn deprecated_variables_still_work_and_warn() {
        let c = load(None, &[("BETTER_SHIFT_ENTER", "1"), ("BETTER_INLINE_IMAGES", "none")], &[]);
        assert_eq!(c.shift_enter(), ShiftEnter::On);
        assert_eq!(*c.source("editing.shift_enter"), Source::Env("BETTER_SHIFT_ENTER"));
        assert_eq!(c.inline_plots(), InlinePlots::None);
        assert_eq!(
            c.warnings,
            [
                "BETTER_INLINE_IMAGES is deprecated: use the setting `startup.inline_plots` instead",
                "BETTER_SHIFT_ENTER is deprecated: use the setting `editing.shift_enter` instead",
            ]
        );
        // Empty means unset; R_HOME isn't deprecated.
        let c = load(None, &[("BETTER_INLINE_IMAGES", ""), ("R_HOME", "/r")], &[]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        // A flag beats the variable.
        let c = load(None, &[("BETTER_SHIFT_ENTER", "1")], &[set("editing.shift_enter=off")]);
        assert_eq!(c.shift_enter(), ShiftEnter::Off);
    }

    #[test]
    fn describe_names_each_source() {
        let c = load(Some("[startup]\nbanner = false\n"), &[("R_HOME", "/r")], &[set("completion.popup=tab")]);
        let text = c.describe();
        assert!(text.contains("startup.banner = false"), "{text}");
        assert!(text.contains("# file"), "{text}");
        assert!(text.contains("# env R_HOME"), "{text}");
        assert!(text.contains("completion.popup = \"tab\""), "{text}");
        assert!(text.contains("# flag --set"), "{text}");
        assert!(text.contains("history.size = 10000"), "{text}");
        assert_eq!(text.lines().count(), KEYS.len() + 1);
    }

    #[test]
    fn template_is_written_once_and_reset_keeps_a_backup() {
        let dir = std::env::temp_dir().join(format!("better-config-{}", std::process::id()));
        let path = dir.join("sub").join("config.toml");
        write_template_if_missing(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
        std::fs::write(&path, "[completion]\nmin_chars = 3\n").unwrap();
        write_template_if_missing(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[completion]\nmin_chars = 3\n");
        assert!(reset(&path, true).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
        assert_eq!(std::fs::read_to_string(dir.join("sub").join("config.toml.bak")).unwrap(), "[completion]\nmin_chars = 3\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
