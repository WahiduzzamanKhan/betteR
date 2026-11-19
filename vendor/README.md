# Vendored crates

## crossterm 0.29.0

The crates.io release, unchanged except for one fix in
`src/event/sys/windows/parse.rs` (marked "betteR patch"), taken from the open
upstream pull request
[crossterm-rs/crossterm#1073](https://github.com/crossterm-rs/crossterm/pull/1073).

On Windows, the emoji panel and pasting deliver each half of a UTF-16
surrogate pair as a key-down and a key-up. crossterm paired the key-ups too, so
emoji and other characters outside the Basic Multilingual Plane were dropped
([#1072](https://github.com/crossterm-rs/crossterm/issues/1072),
[#848](https://github.com/crossterm-rs/crossterm/issues/848)).

Used through `[patch.crates-io]` in `Cargo.toml`, so reedline gets it too.
Delete this directory and that entry once a crossterm release includes the fix.
crossterm is MIT-licensed; its licence is in `crossterm/LICENSE`.
