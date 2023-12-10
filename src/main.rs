use std::ffi::OsString;

use argh::FromArgs;

mod ui;

/// Interactive text UI for searching file contents.
///
/// Start typing a regular expression and see the results in real time. Use
/// Ctrl-C to clear the current search and Esc to quit the program.
#[derive(Debug, FromArgs)]
struct Opts {
    /// pattern to search for
    #[argh(option, short = 'p')]
    pattern: Option<String>,

    /// list of paths to search
    #[argh(positional)]
    search_paths: Vec<OsString>,
}

fn main() -> eyre::Result<()> {
    let opts: Opts = argh::from_env();

    let search_paths =
        if !opts.search_paths.is_empty() { opts.search_paths } else { vec![OsString::from(".")] };

    let mut app = ui::App::new(search_paths, opts.pattern)?;
    app.render()?;

    Ok(())
}
