use std::collections::BTreeSet;
use std::env;
use std::io;
use std::time::Duration;

use crossterm::event;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use svit::{AgentModel, ContentPart, Message, MessageRole, Svit, Value};
use tokio::sync::broadcast;
use tuika::prelude::*;
use tuika::probe::RectProbe;
use tuika_codeformatters::TreeSitterHighlighter;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const MEMORY_WIDTH: u16 = 30;
const FRAME_TIME: Duration = Duration::from_millis(50);
const MAX_PREVIEW_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_ITEMS: usize = 200;

fn lampa_theme() -> Theme {
    let text = Color::Rgb(230, 230, 232);
    let muted = Color::Rgb(140, 140, 145);
    let dim = Color::Rgb(72, 72, 78);
    let blue = Color::Rgb(45, 91, 158);
    let gold = Color::Rgb(126, 94, 19);
    Theme {
        background: Color::Reset,
        surface: Color::Rgb(28, 28, 34),
        text,
        muted,
        dim,
        accent: blue,
        accent_alt: gold,
        border: dim,
        border_focused: blue,
        selection_bg: blue,
        selection_fg: text,
        // Keep Lampa visually aligned with Yolop's native Tuika theme.
        code: tuika::style::CodeTheme {
            heading: text,
            link: blue,
            background: Color::Rgb(18, 18, 20),
            text,
            label: dim,
            keyword: gold,
            function: blue,
            type_name: Color::Rgb(126, 170, 176),
            constant: Color::Rgb(184, 152, 120),
            string: Color::Rgb(132, 166, 142),
            comment: muted,
            punctuation: dim,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Composer,
    Memory,
    Preview,
}

#[derive(Clone, Copy, Debug, Default)]
struct PanelBounds {
    conversation: Rect,
    memory: Rect,
    preview: Rect,
}

impl PanelBounds {
    fn focus_at(self, mouse: &Mouse) -> Option<Focus> {
        if contains(self.conversation, mouse) {
            Some(Focus::Composer)
        } else if contains(self.memory, mouse) {
            Some(Focus::Memory)
        } else if contains(self.preview, mouse) {
            Some(Focus::Preview)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default)]
struct UiProbes {
    conversation: RectProbe,
    memory: RectProbe,
    preview: RectProbe,
    composer: RectProbe,
}

fn contains(rect: Rect, mouse: &Mouse) -> bool {
    mouse.column >= rect.x
        && mouse.column < rect.right()
        && mouse.row >= rect.y
        && mouse.row < rect.bottom()
}

#[derive(Clone, Debug, PartialEq)]
struct TreeRow {
    label: String,
    path: String,
    locator: Vec<PathPart>,
    expandable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathPart {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug)]
enum TimelineEntry {
    Message(Box<Message>),
    Event { label: &'static str, text: String },
}

struct PreviewCache {
    path: String,
    width: u16,
    lines: Vec<Line<'static>>,
}

struct App {
    focus: Focus,
    tree: SelectState,
    root: Value,
    expanded: BTreeSet<String>,
    memory_viewport_height: usize,
    panel_bounds: PanelBounds,
    composer: TextInputState,
    transcript_scroll: ScrollState,
    preview_scroll: ScrollState,
    preview_content_height: usize,
    preview_cache: Option<PreviewCache>,
    rows: Vec<TreeRow>,
    timeline: Vec<TimelineEntry>,
    working: bool,
    failure: Option<String>,
    version: u64,
    model: String,
}

impl App {
    fn new(root: Value, version: u64, model: String) -> Self {
        let mut preview_scroll = ScrollState::new();
        preview_scroll.jump_to_top();
        let expanded = expandable_paths(&root);
        let rows = tree_rows(&root, &expanded);
        Self {
            focus: Focus::Composer,
            tree: SelectState::new(),
            root,
            expanded,
            memory_viewport_height: 1,
            panel_bounds: PanelBounds::default(),
            composer: TextInputState::new(),
            transcript_scroll: ScrollState::new(),
            preview_scroll,
            preview_content_height: 1,
            preview_cache: None,
            rows,
            timeline: Vec::new(),
            working: false,
            failure: None,
            version,
            model,
        }
    }

    fn selected(&self) -> &TreeRow {
        let index = self.tree.selected().unwrap_or(0).min(self.rows.len() - 1);
        &self.rows[index]
    }

    fn selected_value(&self) -> &Value {
        self.selected()
            .locator
            .iter()
            .fold(&self.root, |value, part| match (value, part) {
                (Value::Map(values), PathPart::Key(key)) => &values[key],
                (Value::Array(values), PathPart::Index(index)) => &values[*index],
                _ => unreachable!("tree locators are built from the current root"),
            })
    }

    fn refresh_process(&mut self, root: Value, version: u64) {
        let selected_path = self.selected().path.clone();
        self.root = root;
        self.rows = tree_rows(&self.root, &self.expanded);
        self.preview_cache = None;
        let selected = self.visible_index_or_ancestor(&selected_path);
        self.tree.select(Some(selected));
        self.version = version;
    }

    fn visible_index_or_ancestor(&self, path: &str) -> usize {
        let mut candidate = path;
        loop {
            if let Some(index) = self.rows.iter().position(|row| row.path == candidate) {
                return index;
            }
            candidate = candidate
                .rsplit_once('/')
                .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                .unwrap_or("/");
        }
    }

    fn rebuild_tree(&mut self, selected_path: &str) {
        self.rows = tree_rows(&self.root, &self.expanded);
        self.tree
            .select(Some(self.visible_index_or_ancestor(selected_path)));
    }

    fn toggle_selected(&mut self) {
        let row = self.selected();
        if !row.expandable {
            return;
        }
        let path = row.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
        }
        self.rebuild_tree(&path);
    }

    fn collapse_or_select_parent(&mut self) {
        let path = self.selected().path.clone();
        if self.expanded.remove(&path) {
            self.rebuild_tree(&path);
            return;
        }
        let locator = &self.selected().locator;
        if locator.is_empty() {
            return;
        }
        let parent = &locator[..locator.len() - 1];
        if let Some(index) = self.rows.iter().position(|row| row.locator == parent) {
            self.tree.select(Some(index));
        }
    }

    fn expand_selected(&mut self) {
        let row = self.selected();
        if row.expandable && !self.expanded.contains(&row.path) {
            let path = row.path.clone();
            self.expanded.insert(path.clone());
            self.rebuild_tree(&path);
        }
    }

    fn move_tree(&mut self, distance: usize, down: bool) {
        let selected = self.tree.selected().unwrap_or(0);
        let next = if down {
            selected.saturating_add(distance).min(self.rows.len() - 1)
        } else {
            selected.saturating_sub(distance)
        };
        self.tree.select(Some(next));
    }

    fn push_user(&mut self, text: String) {
        self.timeline
            .push(TimelineEntry::Message(Box::new(Message::user(text))));
        self.working = true;
    }

    fn push_assistant(&mut self, message: Message) {
        self.timeline
            .push(TimelineEntry::Message(Box::new(message)));
        self.working = false;
    }

    fn push_error(&mut self, error: String) {
        self.timeline.push(TimelineEntry::Event {
            label: "ERROR",
            text: format!("Agent loop failed: {error}"),
        });
        self.working = false;
        self.failure = Some(error);
    }

    fn capture_panel_bounds(&mut self, probes: &UiProbes) {
        self.panel_bounds = PanelBounds {
            conversation: probes.conversation.rect(),
            memory: probes.memory.rect(),
            preview: probes.preview.rect(),
        };
    }

    fn route_mouse(&mut self, event: &Event) -> bool {
        let Event::Mouse(mouse) = event else {
            return false;
        };
        let Some(target) = self.panel_bounds.focus_at(mouse) else {
            return false;
        };
        match mouse.kind {
            MouseKind::Down(MouseButton::Left) if mouse.plain() => {
                self.focus = target;
                true
            }
            MouseKind::ScrollUp | MouseKind::ScrollDown if mouse.plain() => {
                self.focus = target;
                let down = mouse.kind == MouseKind::ScrollDown;
                match target {
                    Focus::Memory => {
                        let selected = self.tree.selected();
                        self.move_tree(3, down);
                        if self.tree.selected() != selected {
                            self.preview_scroll.jump_to_top();
                        }
                    }
                    Focus::Composer => {
                        let _ = self.transcript_scroll.handle(
                            event,
                            self.timeline.len() * 3,
                            usize::from(self.panel_bounds.conversation.height.saturating_sub(3)),
                        );
                    }
                    Focus::Preview => {
                        let _ = self.preview_scroll.handle(
                            event,
                            self.preview_content_height,
                            usize::from(self.panel_bounds.preview.height.saturating_sub(2)),
                        );
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn take_submission(&mut self) -> Option<String> {
        let text = self.composer.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let submitted = trimmed.to_owned();
        self.composer.clear();
        Some(submitted)
    }

    fn handle(&mut self, event: &Event) -> AppAction {
        if matches!(
            event,
            Event::Key(Key {
                code: KeyCode::Char('c'),
                ctrl: true,
                ..
            })
        ) {
            return AppAction::Quit;
        }

        if matches!(
            event,
            Event::Key(Key {
                code: KeyCode::Tab,
                ..
            })
        ) {
            self.focus = match self.focus {
                Focus::Composer => Focus::Memory,
                Focus::Memory => Focus::Preview,
                Focus::Preview => Focus::Composer,
            };
            return AppAction::Continue;
        }

        if self.route_mouse(event) {
            return AppAction::Continue;
        }

        match self.focus {
            Focus::Memory => {
                let selected = self.tree.selected();
                match event {
                    Event::Key(Key {
                        code: KeyCode::Enter,
                        ..
                    }) => self.toggle_selected(),
                    Event::Key(Key {
                        code: KeyCode::Left,
                        ..
                    }) => self.collapse_or_select_parent(),
                    Event::Key(Key {
                        code: KeyCode::Right,
                        ..
                    }) => self.expand_selected(),
                    Event::Key(Key {
                        code: KeyCode::PageUp,
                        ..
                    }) => {
                        self.move_tree(self.memory_viewport_height.saturating_sub(1).max(1), false)
                    }
                    Event::Key(Key {
                        code: KeyCode::PageDown,
                        ..
                    }) => {
                        self.move_tree(self.memory_viewport_height.saturating_sub(1).max(1), true)
                    }
                    Event::Mouse(Mouse {
                        kind: MouseKind::ScrollUp,
                        ..
                    }) => self.move_tree(3, false),
                    Event::Mouse(Mouse {
                        kind: MouseKind::ScrollDown,
                        ..
                    }) => self.move_tree(3, true),
                    _ => {
                        let _ = self.tree.handle_with(
                            event,
                            self.rows.len(),
                            SelectNavigation {
                                vim: true,
                                ..SelectNavigation::default()
                            },
                        );
                    }
                }
                if self.tree.selected() != selected {
                    self.preview_scroll.jump_to_top();
                }
            }
            Focus::Composer => {
                if matches!(
                    event,
                    Event::Key(Key {
                        code: KeyCode::Esc,
                        ..
                    })
                ) {
                    self.focus = Focus::Memory;
                } else if self.composer_outcome(event) == InputOutcome::Submitted
                    && let Some(text) = self.take_submission()
                {
                    return AppAction::Submit(text);
                } else {
                    let _ = self
                        .transcript_scroll
                        .handle(event, self.timeline.len() * 3, 20);
                }
            }
            Focus::Preview => {
                let _ = self
                    .preview_scroll
                    .handle(event, self.preview_content_height, 20);
            }
        }
        AppAction::Continue
    }

    fn composer_outcome(&mut self, event: &Event) -> InputOutcome {
        match event {
            Event::Key(Key {
                code: KeyCode::Char('\n' | '\r'),
                ..
            }) => self.composer.handle_enter(true),
            _ => self.composer.handle(event),
        }
    }

    fn cursor(&self, probes: &UiProbes) -> Option<(u16, u16)> {
        (self.focus == Focus::Composer).then(|| self.composer.cursor_screen(probes.composer.rect()))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AppAction {
    Continue,
    Submit(String),
    Quit,
}

pub async fn run(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let model = parse_model(&mut arguments)?;
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for Lampa".to_string())?;

    let mut svit = Svit::builder("svit://local/lampa/process")
        .map_err(|error| error.to_string())?
        .name("lampa")
        .system_prompt(
            "You own this Svit process. Use its memory tree for durable facts and working state.",
        )
        .model(AgentModel::openai(&model, api_key))
        .build()
        .await
        .map_err(|error| error.to_string())?;
    let root = svit
        .read("/")
        .map_err(|error| error.to_string())?
        .expect("a process always has a root");
    let mut app = App::new(
        root,
        svit.version().map_err(|error| error.to_string())?,
        model,
    );
    let inbox = svit.inbox();
    let mut outbox = svit.outbox();
    let mut errors = svit.errors();
    svit.start().map_err(|error| error.to_string())?;

    let ui_result = run_terminal(&mut app, &svit, &inbox, &mut outbox, &mut errors);
    svit.block().await.map_err(|error| error.to_string())?;
    ui_result
}

fn parse_model(arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    match arguments.next() {
        None => Ok(env::var("SVIT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())),
        Some(flag) if flag == "--model" => {
            let model = arguments
                .next()
                .ok_or_else(|| "--model requires a value".to_string())?;
            if arguments.next().is_some() {
                return Err("usage: lampa [--model <model>]".into());
            }
            Ok(model)
        }
        Some(_) => Err("usage: lampa [--model <model>]".into()),
    }
}

fn run_terminal(
    app: &mut App,
    svit: &Svit,
    inbox: &svit::Inbox,
    outbox: &mut broadcast::Receiver<Message>,
    errors: &mut broadcast::Receiver<String>,
) -> Result<(), String> {
    let theme = lampa_theme();
    let probes = UiProbes::default();
    let _session = TerminalSession::enter().map_err(|error| error.to_string())?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .map_err(|error| error.to_string())?;

    let result = (|| {
        loop {
            while let Ok(message) = outbox.try_recv() {
                app.push_assistant(message);
            }
            while let Ok(error) = errors.try_recv() {
                app.push_error(error);
            }
            let root = svit
                .read("/")
                .map_err(|error| error.to_string())?
                .expect("a process always has a root");
            app.refresh_process(root, svit.version().map_err(|error| error.to_string())?);

            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let root = build_view(app, area, &theme, &probes);
                    paint(frame.buffer_mut(), area, &theme, root.as_ref(), &[]);
                    if let Some(position) = app.cursor(&probes) {
                        frame.set_cursor_position(position);
                    }
                })
                .map_err(|error| error.to_string())?;
            app.capture_panel_bounds(&probes);

            if event::poll(FRAME_TIME).map_err(|error| error.to_string())?
                && let Some(event) =
                    translate_event(event::read().map_err(|error| error.to_string())?)
            {
                match app.handle(&event) {
                    AppAction::Continue => {}
                    AppAction::Submit(text) => {
                        inbox
                            .send(Message::user(text.clone()))
                            .map_err(|error| error.to_string())?;
                        app.push_user(text);
                    }
                    AppAction::Quit => break,
                }
            }
        }
        Ok(())
    })();
    let _ = terminal.clear();
    result
}

fn has_children(value: &Value) -> bool {
    match value {
        Value::Map(values) => !values.is_empty(),
        Value::Array(values) => !values.is_empty(),
        _ => false,
    }
}

fn expandable_paths(root: &Value) -> BTreeSet<String> {
    fn collect(value: &Value, path: &str, paths: &mut BTreeSet<String>) {
        if !has_children(value) {
            return;
        }
        paths.insert(path.to_owned());
        match value {
            Value::Map(values) => {
                for (name, child) in values {
                    let child_path = if path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{path}/{name}")
                    };
                    collect(child, &child_path, paths);
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    let child_path = if path == "/" {
                        format!("/{index}")
                    } else {
                        format!("{path}/{index}")
                    };
                    collect(child, &child_path, paths);
                }
            }
            _ => {}
        }
    }

    let mut paths = BTreeSet::new();
    collect(root, "/", &mut paths);
    paths
}

fn tree_rows(root: &Value, expanded: &BTreeSet<String>) -> Vec<TreeRow> {
    let root_marker = if has_children(root) {
        if expanded.contains("/") {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "  "
    };
    let mut rows = vec![TreeRow {
        label: format!("{root_marker}/"),
        path: "/".into(),
        locator: Vec::new(),
        expandable: has_children(root),
    }];
    if expanded.contains("/") {
        flatten_memory(root, "", "", true, &[], expanded, &mut rows);
    }
    rows
}

fn flatten_memory(
    value: &Value,
    path: &str,
    indent: &str,
    last_parent: bool,
    parent_locator: &[PathPart],
    expanded: &BTreeSet<String>,
    rows: &mut Vec<TreeRow>,
) {
    let children: Vec<(String, PathPart, &Value)> = match value {
        Value::Map(values) => values
            .iter()
            .map(|(name, value)| (name.clone(), PathPart::Key(name.clone()), value))
            .collect(),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| (format!("[{index}]"), PathPart::Index(index), value))
            .collect(),
        _ => return,
    };
    let next_indent = format!("{indent}{}", if last_parent { "  " } else { "│ " });
    let child_count = children.len();
    for (index, (name, part, child)) in children.into_iter().enumerate() {
        let last = index + 1 == child_count;
        let branch = if last { "└─" } else { "├─" };
        let child_path = match &part {
            PathPart::Index(child_index) => format!("{path}/{child_index}"),
            PathPart::Key(_) => format!("{path}/{name}"),
        };
        let marker = if has_children(child) {
            if expanded.contains(&child_path) {
                "▾ "
            } else {
                "▸ "
            }
        } else {
            "  "
        };
        let mut locator = parent_locator.to_vec();
        locator.push(part);
        rows.push(TreeRow {
            label: format!("{next_indent}{branch}{marker}{name}"),
            path: child_path.clone(),
            locator: locator.clone(),
            expandable: has_children(child),
        });
        if expanded.contains(&child_path) {
            flatten_memory(
                child,
                &child_path,
                &next_indent,
                last,
                &locator,
                expanded,
                rows,
            );
        }
    }
}

fn build_view(app: &mut App, area: Rect, theme: &Theme, probes: &UiProbes) -> Element {
    let memory_width = MEMORY_WIDTH.min(area.width.saturating_sub(48)).max(20);
    let preview_width = area.width.saturating_sub(memory_width) / 3;
    app.memory_viewport_height = usize::from(area.height.saturating_sub(3)).max(1);
    let preview_content_width = preview_width.saturating_sub(4).max(1);
    let selected_path = app.selected().path.clone();
    let cache_matches = app
        .preview_cache
        .as_ref()
        .is_some_and(|cache| cache.path == selected_path && cache.width == preview_content_width);
    if !cache_matches {
        let lines = preview_lines(
            &selected_path,
            app.selected_value(),
            preview_content_width,
            theme,
        );
        app.preview_cache = Some(PreviewCache {
            path: selected_path.clone(),
            width: preview_content_width,
            lines,
        });
    }
    let body = Flex::row()
        .grow(
            2,
            probes
                .conversation
                .wrap(conversation_view(app, theme, &probes.composer)),
        )
        .fixed(memory_width, probes.memory.wrap(tree_view(app, theme)))
        .grow(
            1,
            probes.preview.wrap(memory_preview(
                app,
                selected_path,
                area.height.saturating_sub(3).max(1),
                theme,
            )),
        );
    element(
        Flex::column()
            .grow(1, element(body))
            .fixed(1, status_view(app, theme)),
    )
}

fn tree_view(app: &App, theme: &Theme) -> Element {
    let lines = app
        .rows
        .iter()
        .map(|row| Line::from(row.label.clone()))
        .collect();
    let border = if app.focus == Focus::Memory {
        theme.border_focused
    } else {
        theme.border
    };
    element(
        Boxed::new(element(
            SelectList::new(lines, &app.tree)
                .viewport(app.memory_viewport_height.min(usize::from(u16::MAX)) as u16)
                .scrollbar(true),
        ))
        .title(" Memory ")
        .border_color(border)
        .padding(Padding::symmetric(0, 0)),
    )
}

fn conversation_view(app: &mut App, theme: &Theme, probe: &RectProbe) -> Element {
    let lines = timeline_lines(&app.timeline, theme);
    let viewport_height = 20usize;
    app.transcript_scroll.clamp(lines.len(), viewport_height);
    let transcript = element(Scroll::new(lines, &app.transcript_scroll).wrap(true));

    let input = element(
        TextInput::new(&app.composer)
            .placeholder("Send a message…", theme.muted_style())
            .style(Style::default().fg(theme.text)),
    );
    let border = if app.focus == Focus::Composer {
        theme.border_focused
    } else {
        theme.border
    };
    let composer_height = app.composer.visual_height(60).clamp(1, 5);
    let content = Flex::column()
        .grow(1, transcript)
        .fixed(
            1,
            element(
                Rule::new()
                    .title(" Inbox ")
                    .style(Style::default().fg(theme.border)),
            ),
        )
        .fixed(composer_height, probe.wrap(input));
    element(
        Boxed::new(element(content))
            .border_color(border)
            .padding(Padding::symmetric(1, 0)),
    )
}

fn memory_preview(app: &mut App, path: String, viewport_height: u16, theme: &Theme) -> Element {
    let lines = &app
        .preview_cache
        .as_ref()
        .expect("build_view populates the selected preview")
        .lines;
    app.preview_content_height = lines.len();
    app.preview_scroll
        .clamp(lines.len(), usize::from(viewport_height));
    let start = app.preview_scroll.offset();
    let end = start
        .saturating_add(usize::from(viewport_height))
        .min(lines.len());
    let window = lines[start..end].to_vec();
    let border = if app.focus == Focus::Preview {
        theme.border_focused
    } else {
        theme.border
    };
    element(
        Boxed::new(element(Scroll::windowed(
            window,
            lines.len(),
            &app.preview_scroll,
        )))
        .title(format!(" {path} "))
        .border_color(border)
        .padding(Padding::symmetric(1, 0)),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum PreviewFormat {
    Markdown,
    Json,
    Code(&'static str),
    Summary,
}

struct PreviewDocument {
    format: PreviewFormat,
    source: String,
}

fn preview_document(path: &str, value: &Value) -> PreviewDocument {
    match value {
        Value::String(text) => {
            let (source, truncated) = bounded_preview_source(text);
            if !truncated && let Ok(json) = serde_json::from_str::<serde_json::Value>(&source) {
                return PreviewDocument {
                    format: PreviewFormat::Json,
                    source: serde_json::to_string_pretty(&json)
                        .expect("parsed JSON always serializes"),
                };
            }
            if let Some(language) = detect_code(path, &source) {
                return PreviewDocument {
                    format: PreviewFormat::Code(language),
                    source,
                };
            }
            PreviewDocument {
                format: PreviewFormat::Markdown,
                source,
            }
        }
        Value::Script(script) => {
            let (source, _) = bounded_preview_source(script.source());
            PreviewDocument {
                format: PreviewFormat::Code("lisp"),
                source,
            }
        }
        Value::Map(values) => PreviewDocument {
            format: PreviewFormat::Summary,
            source: container_summary(
                "object",
                values.len(),
                values
                    .iter()
                    .map(|(key, value)| (key.as_str(), value_kind(value))),
            ),
        },
        Value::Array(values) => PreviewDocument {
            format: PreviewFormat::Summary,
            source: container_summary(
                "array",
                values.len(),
                values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (index.to_string(), value_kind(value))),
            ),
        },
        value => PreviewDocument {
            format: PreviewFormat::Json,
            source: serde_json::to_string_pretty(&value.to_json())
                .expect("persistent values always serialize to JSON"),
        },
    }
}

fn bounded_preview_source(source: &str) -> (String, bool) {
    if source.len() <= MAX_PREVIEW_BYTES {
        return (source.to_owned(), false);
    }
    let mut end = MAX_PREVIEW_BYTES;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!(
            "{}\n\n… preview truncated at {} KiB …",
            &source[..end],
            MAX_PREVIEW_BYTES / 1024
        ),
        true,
    )
}

fn container_summary<K: AsRef<str>>(
    kind: &str,
    count: usize,
    children: impl Iterator<Item = (K, &'static str)>,
) -> String {
    let mut summary = format!("{kind} · {count} items");
    for (name, child_kind) in children.take(MAX_PREVIEW_ITEMS) {
        summary.push_str(&format!("\n{}  {child_kind}", name.as_ref()));
    }
    if count > MAX_PREVIEW_ITEMS {
        summary.push_str(&format!(
            "\n… {} more items; expand the memory tree to inspect them …",
            count - MAX_PREVIEW_ITEMS
        ));
    }
    summary
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Integer(_) | Value::Number(_) => "number",
        Value::String(_) => "text",
        Value::Array(_) => "array",
        Value::Map(_) => "object",
        Value::Script(_) => "script",
    }
}

fn detect_code(path: &str, source: &str) -> Option<&'static str> {
    let text = source.trim_start();
    if text.starts_with("```") {
        return None;
    }
    if text.starts_with("#!") {
        return Some("shell");
    }
    if text.starts_with("<!DOCTYPE html") || text.starts_with("<html") {
        return Some("html");
    }
    if text.starts_with("package ") && text.contains("func ") {
        return Some("go");
    }
    if text.starts_with("def ")
        || text.starts_with("from ") && text.contains(" import ")
        || text.starts_with("import ") && !text.contains(';')
    {
        return Some("python");
    }
    if text.contains("fn ")
        && (text.contains("let ") || text.contains("impl ") || text.contains("use "))
    {
        return Some("rust");
    }
    if text.contains("function ")
        || text.contains("const ")
        || text.contains("let ") && text.contains("=>")
        || text.contains("interface ")
    {
        return Some("typescript");
    }
    let uppercase = text.to_ascii_uppercase();
    if ["SELECT ", "INSERT ", "UPDATE ", "DELETE ", "CREATE TABLE "]
        .iter()
        .any(|prefix| uppercase.starts_with(prefix))
    {
        return Some("sql");
    }
    if path.ends_with("/source") || text.starts_with("(define ") || text.starts_with("(lambda ") {
        return Some("lisp");
    }
    None
}

#[derive(Default)]
struct PreviewHighlighter(TreeSitterHighlighter);

impl Highlighter for PreviewHighlighter {
    fn highlight(
        &self,
        language: &str,
        lines: &[&str],
        theme: &Theme,
    ) -> Option<Vec<Vec<Span<'static>>>> {
        if language.eq_ignore_ascii_case("json") {
            return Some(
                lines
                    .iter()
                    .map(|line| highlight_json_line(line, theme))
                    .collect(),
            );
        }
        self.0.highlight(language, lines, theme)
    }
}

fn highlight_json_line(line: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let byte = bytes[start];
        let (end, color) = match byte {
            b' ' | b'\t' => {
                let mut end = start + 1;
                while end < bytes.len() && matches!(bytes[end], b' ' | b'\t') {
                    end += 1;
                }
                (end, theme.code.text)
            }
            b'"' => {
                let mut end = start + 1;
                let mut escaped = false;
                while end < bytes.len() {
                    let current = bytes[end];
                    end += 1;
                    if current == b'"' && !escaped {
                        break;
                    }
                    escaped = current == b'\\' && !escaped;
                    if current != b'\\' {
                        escaped = false;
                    }
                }
                let key = line[end..].trim_start().starts_with(':');
                (
                    end,
                    if key {
                        theme.code.function
                    } else {
                        theme.code.string
                    },
                )
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => (start + 1, theme.code.punctuation),
            b'-' | b'0'..=b'9' => {
                let mut end = start + 1;
                while end < bytes.len()
                    && matches!(bytes[end], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    end += 1;
                }
                (end, theme.code.constant)
            }
            _ => {
                let mut end = start + 1;
                while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
                    end += 1;
                }
                (end, theme.code.keyword)
            }
        };
        spans.push(Span::styled(
            line[start..end].to_owned(),
            Style::default().fg(color),
        ));
        start = end;
    }
    spans
}

fn preview_lines(path: &str, value: &Value, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let document = preview_document(path, value);
    let highlighter = PreviewHighlighter::default();
    match document.format {
        PreviewFormat::Summary => document
            .source
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect(),
        PreviewFormat::Json => {
            let lines: Vec<&str> = document.source.lines().collect();
            highlighter
                .highlight("json", &lines, theme)
                .expect("JSON highlighting is always available")
                .into_iter()
                .map(Line::from)
                .collect()
        }
        PreviewFormat::Markdown => tuika::components::markdown::to_lines(
            &document.source,
            width,
            theme,
            &StyleSheet::from_theme(theme),
            CodeHighlighter::With(&highlighter),
        ),
        PreviewFormat::Code(language) => tuika::components::markdown::to_lines(
            &format!("```{language}\n{}\n```", document.source),
            width,
            theme,
            &StyleSheet::from_theme(theme),
            CodeHighlighter::With(&highlighter),
        ),
    }
}

fn timeline_lines(entries: &[TimelineEntry], theme: &Theme) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return vec![Line::from(Span::styled(
            "Start a conversation. Messages are committed through the process inbox.",
            theme.muted_style(),
        ))];
    }
    let mut lines = Vec::new();
    for entry in entries {
        let (label, color) = match entry {
            TimelineEntry::Message(message) => match message.role {
                MessageRole::User => ("YOU", theme.accent),
                MessageRole::Agent => ("SVIT", theme.accent_alt),
                MessageRole::System => ("SYSTEM", theme.muted),
                MessageRole::ToolResult => ("TOOL", theme.muted),
            },
            TimelineEntry::Event { label, .. } => (*label, theme.accent_alt),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        match entry {
            TimelineEntry::Message(message) => {
                for part in &message.content {
                    match part {
                        ContentPart::Text(part) => {
                            lines.extend(part.text.lines().map(|line| Line::from(line.to_owned())));
                        }
                        ContentPart::Image(_) | ContentPart::ImageFile(_) => {
                            lines.push(Line::from("[image]"));
                        }
                        ContentPart::ToolCall(call) => {
                            lines.push(Line::from(format!("[tool: {}]", call.name)));
                        }
                        ContentPart::ToolResult(result) => {
                            lines.push(Line::from(format!(
                                "[tool result: {}]",
                                result.tool_call_id
                            )));
                        }
                    }
                }
            }
            TimelineEntry::Event { text, .. } => {
                lines.extend(text.lines().map(|line| Line::from(line.to_owned())));
            }
        }
        lines.push(Line::default());
    }
    lines
}

fn status_view(app: &App, theme: &Theme) -> Element {
    let focus_hint = match app.focus {
        Focus::Composer => "enter send  shift+enter newline",
        Focus::Memory => "↑↓ move  ←→/enter fold  PgUp/PgDn scroll",
        Focus::Preview => "PgUp/PgDn scroll preview",
    };
    let state = if app.failure.is_some() {
        "failed"
    } else if app.working {
        "working"
    } else {
        "ready"
    };
    element(
        StatusBar::new()
            .left(vec![Span::styled(
                format!(" {focus_hint}  tab next panel  ctrl+c quit"),
                theme.muted_style(),
            )])
            .right(vec![Span::styled(
                format!("v{} · {} · {} ", app.version, app.model, state),
                theme.muted_style(),
            )]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use svit::value;
    use tuika::testing::{grid, render};

    fn layout(app: &mut App, width: u16, height: u16) -> UiProbes {
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(app, Rect::new(0, 0, width, height), &theme, &probes);
        let _ = render(root.as_ref(), width, height, &theme);
        app.capture_panel_bounds(&probes);
        probes
    }

    fn panel_mouse(kind: MouseKind, rect: Rect) -> Event {
        Event::Mouse(Mouse::at(
            kind,
            rect.x + rect.width / 2,
            rect.y + rect.height / 2,
        ))
    }

    #[test]
    fn tree_flattens_complete_process_memory_and_preview_tracks_selection() {
        let mut app = App::new(
            value!({
                "agent": {"messages": [], "events": []},
                "memory": {"profile": {"name": "Ada"}, "scores": [3, 5]}
            }),
            7,
            "test-model".into(),
        );

        assert_eq!(app.rows[0].label, "▾ /");
        assert!(app.rows.iter().any(|row| row.path == "/agent/messages"));
        assert!(app.rows.iter().any(|row| row.path == "/memory"));
        assert!(app.rows.iter().any(|row| row.label.ends_with("name")));
        let name_index = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/profile/name")
            .unwrap();
        app.tree.select(Some(name_index));

        assert_eq!(app.selected().path, "/memory/profile/name");
        assert!(matches!(
            app.selected_value(),
            Value::String(value) if value == "Ada"
        ));
    }

    #[test]
    fn text_memory_preview_renders_markdown_instead_of_json_quoting_it() {
        let theme = lampa_theme();
        let lines = preview_lines(
            "/agent/system_prompt",
            &Value::String("# Heading\n\nA **bold** fact.".into()),
            40,
            &theme,
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("Heading"));
        assert!(rendered.contains("bold"));
        assert!(!rendered.contains("**"));
    }

    #[test]
    fn json_held_in_text_is_pretty_printed_as_json() {
        let theme = lampa_theme();
        let lines = preview_lines(
            "/memory/document",
            &Value::String(r#"{"answer":42,"ok":true}"#.into()),
            40,
            &theme,
        );
        let first = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(first.trim(), "{");
    }

    #[test]
    fn source_text_uses_tree_sitter_code_highlighting() {
        let theme = lampa_theme();
        let lines = preview_lines(
            "/lib/example/source",
            &Value::String("use std::io;\nfn main() { let answer = 42; }".into()),
            60,
            &theme,
        );

        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("fn") && span.style.fg == Some(theme.code.keyword)
        }));
    }

    #[test]
    fn container_preview_is_a_shallow_summary_not_a_subtree_dump() {
        let theme = lampa_theme();
        let value = value!({
            "profile": {"biography": "a large descendant that must not be rendered"},
            "scores": [3, 5]
        });
        let rendered = preview_lines("/memory", &value, 60, &theme)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("object · 2 items"));
        assert!(rendered.contains("profile"));
        assert!(rendered.contains("scores"));
        assert!(!rendered.contains("large descendant"));
    }

    #[test]
    fn large_leaf_preview_is_bounded_before_formatting() {
        let theme = lampa_theme();
        let source = format!("start\n{}\nend", "x".repeat(MAX_PREVIEW_BYTES * 2));
        let rendered = preview_lines("/memory/document", &Value::String(source), 60, &theme)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("preview truncated"));
        assert!(!rendered.contains("end"));
    }

    #[test]
    fn refresh_preserves_selected_memory_path() {
        let mut app = App::new(
            value!({"agent": {}, "memory": {"profile": {"name": "Ada"}}}),
            1,
            "test".into(),
        );
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/profile")
            .unwrap();
        app.tree.select(Some(selected));

        app.refresh_process(
            value!({
                "agent": {"messages": []},
                "memory": {"profile": {"name": "Grace"}, "z": true}
            }),
            2,
        );

        assert_eq!(app.selected().path, "/memory/profile");
        assert_eq!(app.version, 2);
    }

    #[test]
    fn composer_submission_is_trimmed_and_cleared() {
        let mut app = App::new(value!({}), 0, "test".into());
        app.focus = Focus::Composer;
        app.composer.set_text("  remember blue  ");

        let action = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert_eq!(action, AppAction::Submit("remember blue".into()));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn shift_enter_inserts_a_composer_newline_without_submitting() {
        let mut app = App::new(value!({}), 0, "test".into());
        app.composer.set_text("first");
        let mut shift_enter = Key::new(KeyCode::Enter);
        shift_enter.shift = true;

        let action = app.handle(&Event::Key(shift_enter));

        assert_eq!(action, AppAction::Continue);
        assert_eq!(app.composer.text(), "first\n");
    }

    #[test]
    fn raw_terminal_newline_encoding_inserts_a_composer_newline() {
        let mut app = App::new(value!({}), 0, "test".into());
        app.composer.set_text("first");

        let action = app.handle(&Event::Key(Key::new(KeyCode::Char('\n'))));

        assert_eq!(action, AppAction::Continue);
        assert_eq!(app.composer.text(), "first\n");
    }

    #[test]
    fn selecting_another_memory_item_starts_its_preview_at_the_top() {
        let mut app = App::new(value!({"agent": {}, "memory": {}}), 0, "test".into());
        app.focus = Focus::Memory;
        app.preview_scroll.set_offset(10);

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Down)));

        assert_eq!(app.selected().path, "/agent");
        assert_eq!(app.preview_scroll.offset(), 0);
    }

    #[test]
    fn memory_nodes_collapse_expand_and_stay_collapsed_after_refresh() {
        let mut app = App::new(
            value!({"agent": {"messages": ["hello"]}, "memory": {}}),
            0,
            "test".into(),
        );
        app.focus = Focus::Memory;
        let agent = app
            .rows
            .iter()
            .position(|row| row.path == "/agent")
            .unwrap();
        app.tree.select(Some(agent));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert!(app.selected().label.contains("▸ agent"));
        assert!(!app.rows.iter().any(|row| row.path == "/agent/messages"));
        app.refresh_process(
            value!({"agent": {"messages": ["hello", "again"]}, "memory": {}}),
            1,
        );
        assert!(!app.rows.iter().any(|row| row.path == "/agent/messages"));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Right)));

        assert_eq!(app.selected().path, "/agent");
        assert!(app.selected().label.contains("▾ agent"));
        assert!(app.rows.iter().any(|row| row.path == "/agent/messages"));
    }

    #[test]
    fn left_on_a_closed_node_moves_to_its_parent() {
        let mut app = App::new(
            value!({"memory": {"profile": {"name": "Ada"}}}),
            0,
            "test".into(),
        );
        app.focus = Focus::Memory;
        let profile = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/profile")
            .unwrap();
        app.tree.select(Some(profile));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Left)));
        assert_eq!(app.selected().path, "/memory/profile");
        assert!(!app.expanded.contains("/memory/profile"));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Left)));

        assert_eq!(app.selected().path, "/memory");
    }

    #[test]
    fn memory_tree_pages_and_scrolls_with_the_mouse_wheel() {
        let mut app = App::new(
            value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}),
            0,
            "test".into(),
        );
        app.focus = Focus::Memory;
        app.memory_viewport_height = 4;

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageDown)));
        assert_eq!(app.tree.selected(), Some(3));

        let _ = app.handle(&Event::Mouse(Mouse::at(MouseKind::ScrollDown, 0, 0)));
        assert_eq!(app.tree.selected(), Some(6));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageUp)));
        assert_eq!(app.tree.selected(), Some(3));
    }

    #[test]
    fn mouse_wheel_over_memory_routes_to_memory_without_keyboard_focus() {
        let mut app = App::new(
            value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}),
            0,
            "test".into(),
        );
        app.memory_viewport_height = 4;
        let probes = layout(&mut app, 90, 24);

        let _ = app.handle(&panel_mouse(MouseKind::ScrollDown, probes.memory.rect()));

        assert_eq!(app.focus, Focus::Memory);
        assert_eq!(app.tree.selected(), Some(3));
    }

    #[test]
    fn clicking_a_panel_activates_it() {
        let mut app = App::new(value!({"memory": {}}), 0, "test".into());
        let probes = layout(&mut app, 90, 24);

        let _ = app.handle(&panel_mouse(
            MouseKind::Down(MouseButton::Left),
            probes.memory.rect(),
        ));
        assert_eq!(app.focus, Focus::Memory);

        let _ = app.handle(&panel_mouse(
            MouseKind::Down(MouseButton::Left),
            probes.preview.rect(),
        ));
        assert_eq!(app.focus, Focus::Preview);
    }

    #[test]
    fn memory_tree_viewport_follows_an_offscreen_selection() {
        let mut app = App::new(
            value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}),
            0,
            "test".into(),
        );
        app.memory_viewport_height = 4;
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/8")
            .unwrap();
        app.tree.select(Some(selected));
        let theme = lampa_theme();

        let frame = grid(&render(tree_view(&app, &theme).as_ref(), 30, 6, &theme));

        assert!(frame.contains("[8]"));
        assert!(!frame.contains("[0]"));
    }

    #[test]
    fn headless_frame_contains_tree_chat_and_status() {
        let mut app = App::new(
            value!({
                "agent": {"messages": []},
                "memory": {"release": {"color": "blue"}}
            }),
            3,
            "sim".into(),
        );
        app.push_user("Remember the release color.".into());
        app.push_assistant(Message::assistant("Stored in memory."));
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(&mut app, Rect::new(0, 0, 90, 24), &theme, &probes);

        let frame = grid(&render(root.as_ref(), 90, 24, &theme));

        assert!(!frame.contains("Inbox / Outbox"));
        assert!(frame.contains(" Memory "));
        assert!(frame.contains("agent"));
        assert!(frame.contains("memory"));
        assert!(frame.contains("Stored in memory."));
        assert!(frame.contains("v3 · sim · ready"));
    }

    #[test]
    fn agent_failure_is_visible_as_an_event_and_status() {
        let mut app = App::new(value!({}), 3, "sim".into());
        app.working = true;

        app.push_error("model request failed".into());

        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(&mut app, Rect::new(0, 0, 90, 24), &theme, &probes);
        let frame = grid(&render(root.as_ref(), 90, 24, &theme));
        assert!(frame.contains("Agent loop failed:"));
        assert!(frame.contains("model request"));
        assert!(frame.contains("v3 · sim · failed"));
    }
}
