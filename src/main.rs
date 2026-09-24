//! crepath: CLI for Cursor IDE operations not exposed in the UI.
//!
//! This tool is not affiliated with or endorsed by Anysphere, Inc. (Cursor).
//! It accesses locally stored data on your machine for personal use.
//! See DISCLAIMER.md for details.

fn main() {
    if let Err(err) = crepath::cli::run() {
        crepath::ui::report_error(&err);
        std::process::exit(1);
    }
}
