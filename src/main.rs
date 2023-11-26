use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Write},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use crossterm::{
    cursor::MoveTo,
    event::{self, read, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use grep::{
    regex::RegexMatcher,
    searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkError, SinkMatch},
};
use ignore::{DirEntry, WalkBuilder, WalkState};
use parking_lot::{Condvar, Mutex};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Text,
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use unicode_width::UnicodeWidthStr;

const TICK_RATE: Duration = Duration::from_millis(100);

enum UiEvent {
    Input(KeyEvent),
    MatchFound { path: OsString, line: u64, text: String },
    Tick,
}

enum SearchState {
    New { pattern: String, paths: Vec<OsString> },
    InProgress { pattern: String },
    Done,
}

impl SearchState {
    fn is_new(&self) -> bool { matches!(self, SearchState::New { .. }) }
}

struct TxSinkError(String);

impl SinkError for TxSinkError {
    fn error_message<T: std::fmt::Display>(message: T) -> Self { TxSinkError(message.to_string()) }
}

struct TxSink {
    path: OsString,
    tx: Sender<UiEvent>,
}

impl TxSink {
    fn new(path: &OsStr, tx: Sender<UiEvent>) -> Self { TxSink { path: path.to_owned(), tx } }
}

impl Sink for TxSink {
    type Error = TxSinkError;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        let res = String::from_utf8_lossy(mat.bytes());
        let ev = UiEvent::MatchFound {
            path: self.path.clone(),
            line: mat.line_number().unwrap_or_default(),
            text: res.to_string(),
        };
        self.tx.send(ev).map_err(|err| TxSinkError(err.to_string()))?;
        Ok(true)
    }
}

struct Events {
    ui_events: Receiver<UiEvent>,
    search_state: Arc<(Mutex<SearchState>, Condvar)>,
    _input_handle: thread::JoinHandle<()>,
    _result_handle: thread::JoinHandle<()>,
}

impl Events {
    fn new() -> Events {
        let (ui_tx, ui_rx) = bounded(1000);
        let search_state = Arc::new((Mutex::new(SearchState::Done), Condvar::new()));

        let input_handle = {
            let tx = ui_tx.clone();
            thread::spawn(move || {
                let handle_events = || -> Result<()> {
                    let mut last_tick = Instant::now();
                    loop {
                        if event::poll(
                            TICK_RATE.checked_sub(last_tick.elapsed()).unwrap_or_default(),
                        )? {
                            match read()? {
                                TermEvent::Key(ev) => {
                                    tx.send(UiEvent::Input(ev))?;
                                }
                                TermEvent::FocusGained
                                | TermEvent::FocusLost
                                | TermEvent::Mouse(..)
                                | TermEvent::Resize(..) => (), // ignore
                            }
                        }

                        if last_tick.elapsed() >= TICK_RATE {
                            tx.send(UiEvent::Tick)?;
                            last_tick = Instant::now();
                        }
                    }
                };

                if let Err(err) = handle_events() {
                    eprintln!("failed to read event: {}", err);
                    std::process::exit(1);
                }
            })
        };

        let result_handle = {
            let search_state = search_state.clone();
            thread::spawn(move || {
                let handle_search = || -> Result<()> {
                    loop {
                        let (search_pattern, search_paths) = {
                            let (ref search_mutex, ref start_anew) = &*search_state;
                            let mut state = search_mutex.lock();

                            let search_pattern: String;
                            let search_paths: Vec<OsString>;

                            match &*state {
                                SearchState::New { pattern, .. } if pattern.is_empty() => {
                                    *state = SearchState::Done;
                                    continue;
                                }
                                SearchState::New { pattern, paths } => {
                                    search_pattern = pattern.to_string();
                                    search_paths = paths.clone();
                                    *state =
                                        SearchState::InProgress { pattern: pattern.to_string() };
                                }
                                SearchState::InProgress { pattern } => {
                                    unreachable!(
                                        "landed in middle of in-progress search: {}",
                                        pattern
                                    );
                                }
                                SearchState::Done => {
                                    start_anew.wait(&mut state);
                                    continue;
                                }
                            }

                            (search_pattern, search_paths)
                        };

                        // validate once here, so that we can simply unwrap in each parallel worker
                        // later
                        let _ = RegexMatcher::new_line_matcher(&search_pattern)?;

                        let (first, rest) = search_paths.split_first().expect("empty path list");
                        let mut walker = WalkBuilder::new(first);
                        for path in rest {
                            walker.add(path);
                        }
                        walker.build_parallel().run(|| {
                            let tx = ui_tx.clone();
                            let search_pattern = search_pattern.clone();
                            let search_state = search_state.clone();

                            Box::new(move |entry: Result<DirEntry, ignore::Error>| {
                                let (ref search_mutex, _) = &*search_state;

                                let entry = match entry {
                                    Ok(entry) => entry,
                                    Err(err) => {
                                        eprintln!("{}", err);
                                        return WalkState::Skip;
                                    }
                                };

                                if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(true) {
                                    return WalkState::Continue;
                                }

                                if search_mutex.lock().is_new() {
                                    return WalkState::Quit;
                                }

                                let sink = TxSink::new(entry.path().as_os_str(), tx.clone());

                                let matcher =
                                    RegexMatcher::new_line_matcher(&search_pattern).unwrap();
                                let mut searcher = SearcherBuilder::new()
                                    .binary_detection(BinaryDetection::quit(b'\x00'))
                                    .line_number(true)
                                    .build();

                                searcher.search_path(&matcher, entry.path(), sink).unwrap_or_else(
                                    |err| {
                                        eprintln!("{}: {}", entry.path().display(), err.0);
                                    },
                                );

                                WalkState::Continue
                            })
                        });

                        *search_state.0.lock() = SearchState::Done;
                    }
                };

                if let Err(err) = handle_search() {
                    eprintln!("search failed: {}", err);
                    std::process::exit(1);
                }
            })
        };

        Events {
            ui_events: ui_rx,
            search_state,
            _input_handle: input_handle,
            _result_handle: result_handle,
        }
    }

    fn next(&self) -> Result<UiEvent, crossbeam_channel::RecvError> { self.ui_events.recv() }

    fn new_search(&mut self, pattern: &str, paths: &[OsString]) -> Result<()> {
        *self.search_state.0.lock() =
            SearchState::New { pattern: pattern.to_owned(), paths: paths.to_owned() };
        self.search_state.1.notify_one();
        Ok(())
    }
}

struct App {
    pattern: String,
    search_paths: Vec<OsString>,
    results: Vec<String>,
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let pattern = args.next().unwrap_or_default().to_string_lossy().to_string();
    let search_paths = {
        let mut paths = args.collect::<Vec<_>>();
        if paths.is_empty() {
            paths.push(OsString::from("./"));
        }
        paths
    };

    let mut events = Events::new();
    events.new_search(&pattern, &search_paths)?;

    let mut app = App { pattern, search_paths, results: Vec::new() };

    render_ui(&mut app, &mut events)
}

fn render_ui(app: &mut App, events: &mut Events) -> Result<()> {
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    terminal.clear()?;
    loop {
        let mut dimensions = terminal.size()?;

        terminal.draw(|f| {
            dimensions = f.size();

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Length(dimensions.height - 3),
                        Constraint::Length(3),
                        Constraint::Min(1),
                    ]
                    .as_ref(),
                )
                .split(dimensions);

            let results_title = format!("Results ({})", app.results.len());
            let results = app
                .results
                .iter()
                .take(usize::from(dimensions.height) - 3)
                .map(Text::raw)
                .map(ListItem::new)
                .collect::<Vec<_>>();
            let results_list = List::new(results)
                .block(Block::default().borders(Borders::ALL).title(results_title));
            f.render_widget(results_list, chunks[0]);

            let input = Paragraph::new(Text::raw(&app.pattern))
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title("Pattern"));
            f.render_widget(input, chunks[1]);
        })?;

        // cursor
        write!(
            terminal.backend_mut(),
            "{}",
            MoveTo(1 + app.pattern.width() as u16, dimensions.height - 2)
        )?;
        io::stdout().flush()?;

        loop {
            match events.next()? {
                UiEvent::Input(ev) => {
                    let mod_keys_used = !(ev.modifiers == KeyModifiers::NONE
                        || ev.modifiers == KeyModifiers::SHIFT);

                    match ev.code {
                        KeyCode::Char('\n') => {} // ignore
                        KeyCode::Char('c')
                            if ev.kind == KeyEventKind::Press
                                && ev.modifiers == KeyModifiers::CONTROL =>
                        {
                            app.pattern.clear();
                            app.results.clear();
                            break;
                        }
                        KeyCode::Char(ch)
                            if !mod_keys_used
                                && (ev.kind == KeyEventKind::Press
                                    || ev.kind == KeyEventKind::Repeat) =>
                        {
                            app.pattern.push(ch);
                            app.results.clear();
                            //TODO: if the key event kind is Repeat, only trigger a new search when
                            // the key is released
                            events.new_search(&app.pattern, &app.search_paths)?;
                            break;
                        }
                        KeyCode::Backspace
                            if ev.kind == KeyEventKind::Press
                                || ev.kind == KeyEventKind::Repeat =>
                        {
                            app.pattern.pop();
                            app.results.clear();
                            if regex::Regex::new(&app.pattern).is_ok() {
                                events.new_search(&app.pattern, &app.search_paths)?;
                                //TODO: show in pattern block title that
                                // pattern is invalid
                            }
                            break;
                        }
                        KeyCode::Esc => {
                            disable_raw_mode()?;
                            terminal.clear()?;
                            std::process::exit(0);
                        }
                        _ => {} // ignore
                    }
                }

                UiEvent::MatchFound { path, line, text } => {
                    app.results.push(format!("{}:{} {}", path.to_string_lossy(), line, text));
                }

                UiEvent::Tick => {
                    break;
                }
            }
        }
    }
}
