use std::{
    fmt::Write,
    fs::read_to_string,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Style, Stylize},
    widgets::Paragraph,
    Frame, Terminal,
};
use tokio::task::JoinHandle;
use toml::Value;

use hitman::{
    env::{
        find_available_requests, find_environments, find_root_dir, get_target,
        load_env, set_target, update_data,
    },
    extract::extract_variables,
    request::{build_client, do_request},
    substitute::{substitute, SubstituteError},
};

use super::{
    centered,
    keymap::{mapkey, KeyMapping},
    output::{HttpMessage, OutputView},
    progress::Progress,
    prompt::{Prompt, PromptIntent},
    select::{RequestSelector, Select, SelectIntent, SelectItem},
    Component,
};

pub trait Screen {
    fn enter(&self) -> io::Result<()>;
    fn leave(&self) -> io::Result<()>;
}

pub struct App {
    root_dir: PathBuf,
    target: String,
    request_selector: RequestSelector,
    output_view: OutputView,

    state: AppState,
    error: Option<String>,
    should_quit: bool,
}

pub enum AppState {
    Idle,

    PendingValue {
        file_path: String,
        key: String,
        pending_options: Vec<(String, String)>,

        pending_state: PendingState,
    },

    RunningRequest {
        handle: JoinHandle<Result<(HttpMessage, HttpMessage)>>,
        progress: Progress,
    },

    SelectTarget {
        component: Select<String>,
    },
}

pub enum PendingState {
    Prompt { component: Prompt },
    Select { component: Select<Value> },
}

pub enum Intent {
    Quit,
    PrepareRequest(String, Vec<(String, String)>),
    SendRequest {
        file_path: String,
        prepared_request: String,
    },
    AskForValue {
        key: String,
        file_path: String,
        pending_options: Vec<(String, String)>,
        params: AskForValueParams,
    },
    ChangeState(AppState),
    SelectTarget,
    AcceptSelectTarget(String),
    EditRequest,
    ShowError(String),
}

pub enum AskForValueParams {
    Prompt { fallback: Option<String> },
    Select { values: Vec<Value> },
}

impl App {
    pub fn new() -> Result<Self> {
        let root_dir = find_root_dir()?.context("No hitman.toml found")?;

        let target = get_target(&root_dir);

        // FIXME: Need to live update requests

        let reqs = find_available_requests(&root_dir)?;
        let reqs: Vec<String> = reqs
            .iter()
            .filter_map(|p| p.to_str())
            .map(String::from)
            .collect();

        let request_selector = RequestSelector::new(&reqs);

        Ok(Self {
            root_dir,
            target,
            request_selector,
            output_view: OutputView::default(),
            state: AppState::Idle,
            error: None,
            should_quit: false,
        })
    }

    pub async fn run<B, S>(
        &mut self,
        mut terminal: Terminal<B>,
        mut screen: S,
    ) -> Result<()>
    where
        B: Backend,
        S: Screen,
    {
        screen.enter()?;

        while !self.should_quit {
            if let AppState::RunningRequest { handle, .. } = &mut self.state {
                if handle.is_finished() {
                    let (request, response) = handle.await??;
                    self.output_view.update(request, response);
                    self.state = AppState::Idle;
                }
            }

            terminal.draw(|frame| self.render_ui(frame, frame.size()))?;

            let mut pending_intent = self.process_events()?;
            while let Some(intent) = pending_intent {
                pending_intent =
                    match self.dispatch(intent, &mut terminal, &mut screen) {
                        Ok(it) => it,
                        Err(err) => Some(Intent::ShowError(err.to_string())),
                    };
            }
        }

        screen.leave()?;

        Ok(())
    }

    fn dispatch<B, S>(
        &mut self,
        intent: Intent,
        terminal: &mut Terminal<B>,
        screen: &mut S,
    ) -> Result<Option<Intent>>
    where
        B: Backend,
        S: Screen,
    {
        use Intent::*;

        Ok(match intent {
            Quit => {
                self.should_quit = true;
                None
            }
            ChangeState(state) => {
                self.error = None;
                self.state = state;
                None
            }
            PrepareRequest(file_path, options) => {
                self.try_request(file_path, options)?
            }
            SendRequest {
                file_path,
                prepared_request,
            } => self.send_request(file_path, prepared_request)?,
            AskForValue {
                key,
                file_path,
                pending_options,
                params,
            } => match params {
                AskForValueParams::Select { values } => {
                    let component = Select::new(
                        format!("Select substitution value for {{{{{key}}}}}",),
                        key.clone(),
                        values.clone(),
                    );

                    let state = AppState::PendingValue {
                        key,
                        file_path,
                        pending_options,
                        pending_state: PendingState::Select { component },
                    };
                    Some(Intent::ChangeState(state))
                }

                AskForValueParams::Prompt { fallback } => {
                    let component =
                        Prompt::new(format!("Enter value for {key}"))
                            .with_fallback(fallback);

                    let state = AppState::PendingValue {
                        key,
                        file_path,
                        pending_options,
                        pending_state: PendingState::Prompt { component },
                    };
                    Some(Intent::ChangeState(state))
                }
            },
            SelectTarget => {
                let envs = find_environments(&self.root_dir)?;
                let component =
                    Select::new("Select target".into(), "target".into(), envs);

                Some(ChangeState(AppState::SelectTarget { component }))
            }
            AcceptSelectTarget(s) => {
                set_target(&self.root_dir, &s)?;
                self.target = s;
                Some(ChangeState(AppState::Idle))
            }
            EditRequest => {
                if let Some(selected) =
                    self.request_selector.selector.selected_item()
                {
                    let editor = std::env::var("EDITOR")
                        .context("EDITOR environment variable not set")?;

                    screen.leave()?;
                    let _ = std::process::Command::new(editor)
                        .arg(selected)
                        .status();

                    screen.enter()?;
                    terminal.clear()?;
                }
                None
            }
            ShowError(err) => {
                self.error = Some(err);
                None
            }
        })
    }

    fn process_events(&mut self) -> Result<Option<Intent>> {
        // Don't waste so much CPU when idle
        let poll_timeout = match self.state {
            AppState::RunningRequest { .. } => Duration::from_millis(50),
            _ => Duration::from_secs(1),
        };

        if event::poll(poll_timeout)? {
            let event = event::read()?;

            return Ok(self.handle_event(&event));
        }

        Ok(None)
    }

    fn try_request(
        &mut self,
        file_path: String,
        options: Vec<(String, String)>,
    ) -> Result<Option<Intent>> {
        let root_dir = self.root_dir.clone();

        let path = PathBuf::from(file_path.clone());
        let env = load_env(&root_dir, &path, &options)?;

        let intent = match substitute(&read_to_string(path.clone())?, &env) {
            Ok(prepared_request) => Some(Intent::SendRequest {
                file_path,
                prepared_request,
            }),
            Err(err) => match err {
                SubstituteError::MultipleValuesFound { key, values } => {
                    Some(Intent::AskForValue {
                        key,
                        file_path,
                        pending_options: options,
                        params: AskForValueParams::Select { values },
                    })
                }
                SubstituteError::ValueNotFound { key, fallback } => {
                    Some(Intent::AskForValue {
                        key,
                        file_path,
                        pending_options: options,
                        params: AskForValueParams::Prompt { fallback },
                    })
                }
                other_err => Some(Intent::ShowError(other_err.to_string())),
            },
        };

        Ok(intent)
    }

    fn send_request(
        &mut self,
        file_path: String,
        prepared_request: String,
    ) -> Result<Option<Intent>> {
        let root_dir = self.root_dir.clone();
        let file_path = PathBuf::from(file_path);

        let handle = tokio::spawn(async move {
            make_request(&prepared_request, &root_dir, &file_path).await
        });

        let state = AppState::RunningRequest {
            handle,
            progress: Progress,
        };

        Ok(Some(Intent::ChangeState(state)))
    }
}

impl Component for App {
    type Intent = Intent;

    fn render_ui(&mut self, frame: &mut Frame, area: Rect) {
        let layout = Layout::new(
            Direction::Vertical,
            [Constraint::Min(0), Constraint::Length(1)],
        )
        .split(area);

        self.render_main(frame, layout[0]);
        self.render_status(frame, layout[1]);
        self.render_popup(frame);
    }

    fn handle_event(&mut self, event: &Event) -> Option<Self::Intent> {
        use Intent::*;

        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match &mut self.state {
                    AppState::PendingValue {
                        key,
                        file_path,
                        pending_options,
                        pending_state,
                        ..
                    } => {
                        match pending_state {
                            PendingState::Select { component } => {
                                if let Some(intent) =
                                    component.handle_event(&event)
                                {
                                    match intent {
                                        SelectIntent::Abort => {
                                            return Some(ChangeState(
                                                AppState::Idle,
                                            ));
                                        }
                                        SelectIntent::Accept(selected) => {
                                            let value = match selected {
                                                Value::Table(t) => match t.get("value") {
                                                    Some(Value::String(value)) => value.clone(),
                                                    Some(value) => value.to_string(),
                                                    _ => return Some(ShowError(
                                                    format!("Replacement not found: {key}"),
)),
                                                },
                                                other => other.to_string(),
                                            };

                                            // XXX Maybe we can simplify this by emitting
                                            // the pending_state
                                            pending_options
                                                .push((key.clone(), value));
                                            return Some(PrepareRequest(
                                                file_path.clone(),
                                                pending_options.clone(),
                                            ));
                                        }
                                    }
                                }
                                return None;
                            }
                            PendingState::Prompt { component } => {
                                if let Some(intent) =
                                    component.handle_event(&event)
                                {
                                    match intent {
                                        PromptIntent::Abort => {
                                            return Some(ChangeState(
                                                AppState::Idle,
                                            ));
                                        }
                                        PromptIntent::Accept(value) => {
                                            pending_options
                                                .push((key.clone(), value));
                                            return Some(PrepareRequest(
                                                file_path.clone(),
                                                pending_options.clone(),
                                            ));
                                        }
                                    }
                                }
                                return None;
                            }
                        }
                    }

                    AppState::Idle => {
                        if let Some(intent) =
                            self.request_selector.handle_event(&event)
                        {
                            match intent {
                                SelectIntent::Abort => (),
                                SelectIntent::Accept(file_path) => {
                                    return Some(PrepareRequest(
                                        file_path,
                                        Vec::new(),
                                    ));
                                }
                            }
                        }

                        self.output_view.handle_event(&event);

                        match mapkey(&event) {
                            KeyMapping::Editor => {
                                return Some(Intent::EditRequest)
                            }
                            KeyMapping::Abort => return Some(Intent::Quit),
                            KeyMapping::SelectTarget => {
                                return Some(Intent::SelectTarget);
                            }
                            _ => (),
                        }
                    }

                    AppState::RunningRequest { handle, .. } => {
                        if let KeyMapping::Abort = mapkey(&event) {
                            handle.abort();
                            return Some(ChangeState(AppState::Idle));
                        }
                    }

                    AppState::SelectTarget { component } => {
                        if let Some(intent) = component.handle_event(&event) {
                            match intent {
                                SelectIntent::Abort => {
                                    return Some(ChangeState(AppState::Idle));
                                }
                                SelectIntent::Accept(s) => {
                                    return Some(AcceptSelectTarget(s));
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

impl App {
    fn render_main(&mut self, frame: &mut Frame, area: Rect) {
        let layout = Layout::new(
            Direction::Horizontal,
            [Constraint::Max(60), Constraint::Min(1)],
        )
        .split(area);

        self.render_left(frame, layout[0]);

        self.output_view.render_ui(frame, layout[1]);
    }

    fn render_left(&mut self, frame: &mut Frame, area: Rect) {
        self.request_selector.render_ui(frame, area);
    }

    fn render_status(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.inner(&Margin::new(1, 0));

        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Fill(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(self.target.as_str())
                .centered()
                .black()
                .on_cyan(),
            layout[0],
        );

        let status_line = match &self.error {
            Some(msg) => Paragraph::new(msg.clone()).white().on_red(),
            None => Paragraph::new(
                " Ctrl+S: Select target, Ctrl+E: Edit selected request",
            )
            .style(Style::new().dark_gray()),
        };

        frame.render_widget(status_line, layout[1]);
    }

    fn render_popup(&mut self, frame: &mut Frame) {
        let area = frame.size();
        match &mut self.state {
            AppState::PendingValue { pending_state, .. } => match pending_state
            {
                PendingState::Prompt { component, .. } => {
                    let inner_area = centered(area, 30, 30);
                    component.render_ui(frame, inner_area);
                }
                PendingState::Select { component, .. } => {
                    let inner_area = centered(area, 60, 20);
                    component.render_ui(frame, inner_area);
                }
            },

            AppState::SelectTarget { component } => {
                let inner_area = centered(area, 30, 20);
                component.render_ui(frame, inner_area);
            }

            AppState::RunningRequest { progress, .. } => {
                progress.render_ui(frame, frame.size());
            }

            _ => (),
        }
    }
}

async fn make_request(
    buf: &str,
    root_dir: &Path,
    file_path: &Path,
) -> Result<(HttpMessage, HttpMessage)> {
    let client = build_client()?;

    let mut request = HttpMessage::default();
    for line in buf.lines() {
        writeln!(request.header, "> {}", line)?;
    }
    writeln!(request.header)?;

    let (res, _elapsed) = do_request(&client, buf).await?;

    let mut response = HttpMessage::default();
    writeln!(
        response.header,
        "> HTTP/1.1 {} {}",
        res.status().as_u16(),
        res.status().canonical_reason().unwrap_or("")
    )?;
    for (name, value) in res.headers() {
        writeln!(response.header, "< {}: {}", name, value.to_str()?)?;
    }
    writeln!(response.header)?;

    if let Ok(json) = res.json::<serde_json::Value>().await {
        writeln!(response.body, "{}", serde_json::to_string_pretty(&json)?)?;

        let options = vec![];
        let env = load_env(root_dir, file_path, &options)?;
        let vars = extract_variables(&json, &env)?;
        update_data(&vars)?;
    }

    Ok((request, response))
}

impl SelectItem for Value {
    fn text(&self) -> String {
        match self {
            Value::Table(t) => match t.get("name") {
                Some(Value::String(value)) => value.clone(),
                Some(value) => value.to_string(),
                None => t.to_string(),
            },
            other => other.to_string(),
        }
    }
}
