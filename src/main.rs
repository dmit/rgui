use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::{
    cursor::MoveTo,
    event::{poll, read, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use grep::{
    regex::RegexMatcher,
    searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkError, SinkMatch},
};
use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Write},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, Paragraph, Text},
    Terminal,
};
use unicode_width::UnicodeWidthStr;
use walkdir::WalkDir;

const TICK_RATE: Duration = Duration::from_millis(100);

enum Event {
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
    fn is_new(&self) -> bool {
        match self {
            SearchState::New { .. } => true,
            _ => false,
        }
    }
}

struct TxSinkError(String);

impl SinkError for TxSinkError {
    fn error_message<T: std::fmt::Display>(message: T) -> Self {
        TxSinkError(message.to_string())
    }
}

struct TxSink {
    path: OsString,
    tx: Sender<Event>,
}

impl TxSink {
    fn new(path: &OsStr, tx: Sender<Event>) -> Self {
        TxSink { path: path.to_owned(), tx }
    }
}

impl Sink for TxSink {
    type Error = TxSinkError;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        let res = String::from_utf8_lossy(mat.bytes());
        let ev = Event::MatchFound {
            path: self.path.clone(),
            line: mat.line_number().unwrap_or_default(),
            text: res.to_string(),
        };
        self.tx.send(ev).map_err(|err| TxSinkError(err.to_string()))?;
        Ok(true)
    }
}

struct Events {
    rx: Receiver<Event>,
    _search_state: Arc<Mutex<SearchState>>,
    _input_handle: thread::JoinHandle<()>,
    _result_handle: thread::JoinHandle<()>,
    _tick_handle: thread::JoinHandle<()>,
}

impl Events {
    fn new() -> Events {
        let (tx, rx) = unbounded();
        let search_state = Arc::new(Mutex::new(SearchState::Done));

        let input_handle = {
            let tx = tx.clone();
            thread::spawn(move || {
                let handle_events = || -> Result<()> {
                    loop {
                        if poll(TICK_RATE)? {
                            match read()? {
                                TermEvent::Key(ev) => {
                                    tx.send(Event::Input(ev))?;
                                }
                                TermEvent::Mouse(..) => (),  // ignore
                                TermEvent::Resize(..) => (), // ignore
                            }
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
            let tx = tx.clone();
            let search_state = search_state.clone();
            thread::spawn(move || {
                let handle_search = || -> Result<()> {
                    'search: loop {
                        let (search_pattern, search_paths) = {
                            let mut state =
                                search_state.lock().map_err(|_| anyhow!("mutex poisoned"))?;

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
                                    continue;
                                }
                            }

                            (search_pattern, search_paths)
                        };

                        let matcher = RegexMatcher::new_line_matcher(&search_pattern)?;
                        let mut searcher = SearcherBuilder::new()
                            .binary_detection(BinaryDetection::quit(b'\x00'))
                            .line_number(true)
                            .build();

                        for path in search_paths {
                            for entry in WalkDir::new(&path) {
                                let entry = entry?;

                                if !entry.file_type().is_file() {
                                    continue;
                                }

                                if search_state
                                    .lock()
                                    .map_err(|_| anyhow!("mutex poisoned"))?
                                    .is_new()
                                {
                                    continue 'search;
                                }

                                let sink = TxSink::new(&entry.path().as_os_str(), tx.clone());
                                searcher.search_path(&matcher, entry.path(), sink).unwrap_or_else(
                                    |err| {
                                        eprintln!("{}: {}", entry.path().display(), err.0);
                                    },
                                );
                            }
                        }

                        *search_state.lock().map_err(|_| anyhow!("mutex poisoned"))? =
                            SearchState::Done;
                    }
                };

                if let Err(err) = handle_search() {
                    eprintln!("search failed: {}", err);
                    std::process::exit(1);
                }
            })
        };

        let tick_handle = thread::spawn(move || loop {
            if let Err(err) = tx.send(Event::Tick) {
                eprintln!("failed to send tick event: {}", err);
                std::process::exit(1);
            }
            thread::sleep(TICK_RATE);
        });

        Events {
            rx,
            _search_state: search_state,
            _input_handle: input_handle,
            _result_handle: result_handle,
            _tick_handle: tick_handle,
        }
    }

    fn next(&self) -> Result<Event, crossbeam_channel::RecvError> {
        self.rx.recv()
    }

    fn new_search(&mut self, pattern: &str, paths: &[OsString]) -> Result<()> {
        let mut state = self._search_state.lock().map_err(|_| anyhow!("mutex poisoned"))?;
        *state = SearchState::New { pattern: pattern.to_owned(), paths: paths.to_owned() };
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

        terminal.draw(|mut f| {
            dimensions = f.size();

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints(
                    [
                        Constraint::Length(dimensions.height - 3 - 2),
                        Constraint::Length(3),
                        Constraint::Min(1),
                    ]
                    .as_ref(),
                )
                .split(dimensions);

            let results = List::new(app.results.iter().map(|r| Text::raw(r)))
                .block(Block::default().borders(Borders::ALL).title("Results"));
            f.render_widget(results, chunks[0]);

            let text = [Text::raw(&app.pattern)];
            let input = Paragraph::new(text.iter())
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title("Pattern"));
            f.render_widget(input, chunks[1]);
        })?;

        // cursor
        write!(
            terminal.backend_mut(),
            "{}",
            MoveTo(2 + app.pattern.width() as u16, dimensions.height - 3)
        )?;
        io::stdout().flush()?;

        match events.next()? {
            Event::Input(ev) => {
                let mod_keys_used = ev.modifiers & (KeyModifiers::ALT | KeyModifiers::CONTROL)
                    != KeyModifiers::NONE;

                match ev.code {
                    KeyCode::Char('\n') => {} // ignore
                    KeyCode::Char(ch) if !mod_keys_used => {
                        app.pattern.push(ch);
                        app.results.clear();
                        events.new_search(&app.pattern, &app.search_paths)?;
                    }
                    KeyCode::Backspace => {
                        app.pattern.pop();
                        app.results.clear();
                        events.new_search(&app.pattern, &app.search_paths)?;
                    }
                    KeyCode::Esc => {
                        disable_raw_mode()?;
                        terminal.clear()?;
                        std::process::exit(0);
                    }
                    _ => {} // ignore
                }
            }

            Event::MatchFound { path, line, text } => {
                app.results.push(format!("{}:{} {}", path.to_string_lossy(), line, text));
            }

            Event::Tick => {} // do nothing
        }
    }
}
