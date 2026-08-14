use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::Duration;

use crossterm::event;
use ratatui::{Terminal, TerminalOptions, Viewport};
use svit::{
    Change, ContentPart, Events, Inbox, Message, MessageRole, Outbox, Svit, SvitEvent, Value,
};
use tuika::prelude::*;
use tuika::probe::RectProbe;
use tuika::term::hyperlink::HyperlinkBackend;
use tuika_codeformatters::TreeSitterHighlighter;

const MEMORY_WIDTH: u16 = 30;
const FRAME_TIME: Duration = Duration::from_millis(50);
const MAX_PREVIEW_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_ITEMS: usize = 200;
const MAX_PREVIEW_DEPTH: usize = 2;
const MAX_INLINE_VALUE_BYTES: usize = 160;
const MAX_TREE_ITEM_PREVIEW_BYTES: usize = 48;
/// Children shown under one directory row.
const MAX_TREE_CHILDREN: usize = 200;

/// Placeholder for a node the tree has not resolved yet.
static PENDING_VALUE: Value = Value::Null;

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

fn panel_body(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
}

#[derive(Clone, Debug, PartialEq)]
struct TreeRow {
    label: String,
    path: String,
    expandable: bool,
}

/// The process operations the console browses one memory tree with.
///
/// Memory, scripts, system metadata, and mounts all answer the same three
/// questions, so the console has one node interface and no special case for
/// `/mounts`.
trait MemoryView {
    fn discover(&self, path: &str) -> Result<Vec<String>, String>;
    fn stat(&self, path: &str) -> Result<Option<Value>, String>;
    fn read(&self, path: &str) -> Result<Option<Value>, String>;
    fn version(&self) -> u64;
}

impl MemoryView for Svit {
    fn discover(&self, path: &str) -> Result<Vec<String>, String> {
        Svit::discover(self, path).map_err(|error| error.to_string())
    }

    fn stat(&self, path: &str) -> Result<Option<Value>, String> {
        Svit::stat(self, path).map_err(|error| error.to_string())
    }

    fn read(&self, path: &str) -> Result<Option<Value>, String> {
        Svit::read(self, path).map_err(|error| error.to_string())
    }

    fn version(&self) -> u64 {
        Svit::version(self).unwrap_or_default()
    }
}

/// What the console knows about one node before reading its content.
#[derive(Clone, Debug, PartialEq)]
struct Node {
    directory: bool,
    /// `cache`, `local`, or `remote`. The console reads content eagerly only
    /// where the process says it is already resident.
    locality: String,
    /// The `content` fact, such as `object`, `array`, or `text/plain`.
    content: String,
}

impl Node {
    fn from_facts(facts: &Value) -> Self {
        let Value::Map(fields) = facts else {
            return Self::unreachable();
        };
        let text = |fields: &BTreeMap<String, Value>, name: &str| match fields.get(name) {
            Some(Value::String(value)) => Some(value.clone()),
            _ => None,
        };
        let content = match fields.get("facts") {
            Some(Value::Map(facts)) => text(facts, "content").unwrap_or_default(),
            _ => String::new(),
        };
        Self {
            directory: text(fields, "kind").as_deref() == Some("directory"),
            locality: text(fields, "locality").unwrap_or_else(|| "remote".into()),
            content,
        }
    }

    fn missing() -> Self {
        Self {
            directory: false,
            locality: "cache".into(),
            content: String::new(),
        }
    }

    fn unreachable() -> Self {
        Self {
            directory: false,
            locality: "remote".into(),
            content: String::new(),
        }
    }

    fn resident(&self) -> bool {
        self.locality == "cache"
    }

    fn array(&self) -> bool {
        self.content == "array"
    }
}

/// Lazily resolved view of one memory tree.
///
/// Nothing is fetched until it is on screen: a directory is listed when it is
/// expanded, and content is read for the selected node and for rows that need
/// a value to summarize. The console holds no committed root of its own.
#[derive(Default)]
struct MemoryTree {
    nodes: BTreeMap<String, Node>,
    children: BTreeMap<String, Vec<String>>,
    values: BTreeMap<String, Value>,
    truncated: BTreeSet<String>,
}

impl MemoryTree {
    fn clear(&mut self) {
        self.nodes.clear();
        self.children.clear();
        self.values.clear();
        self.truncated.clear();
    }

    /// Forgets every entry the change could have made stale.
    fn invalidate(&mut self, change: &Change) {
        self.nodes.retain(|path, _| !change.touches(path));
        self.children.retain(|path, _| !change.touches(path));
        self.values.retain(|path, _| !change.touches(path));
        self.truncated.retain(|path| !change.touches(path));
    }

    fn resolved(&self) -> usize {
        self.nodes.len() + self.children.len() + self.values.len()
    }

    fn node(&self, path: &str) -> Option<&Node> {
        self.nodes.get(path)
    }

    fn is_directory(&self, path: &str) -> bool {
        self.node(path).is_some_and(|node| node.directory)
    }

    fn children(&self, path: &str) -> &[String] {
        self.children.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    fn value(&self, path: &str) -> Option<&Value> {
        self.values.get(path)
    }

    fn resolve_node(&mut self, view: &dyn MemoryView, path: &str) {
        if self.nodes.contains_key(path) {
            return;
        }
        let node = match view.stat(path) {
            Ok(Some(facts)) => Node::from_facts(&facts),
            Ok(None) => Node::missing(),
            Err(error) => {
                // An unreachable node still occupies a row; its failure becomes
                // the value the preview shows.
                self.values.insert(path.to_owned(), Value::String(error));
                Node::unreachable()
            }
        };
        self.nodes.insert(path.to_owned(), node);
    }

    fn resolve_children(&mut self, view: &dyn MemoryView, path: &str) {
        if self.children.contains_key(path) || !self.is_directory(path) {
            return;
        }
        let mut names = match view.discover(path) {
            Ok(names) => names,
            Err(error) => {
                self.children.insert(path.to_owned(), Vec::new());
                self.values.insert(path.to_owned(), Value::String(error));
                return;
            }
        };
        // A directory has no committed size, so one listing is bounded here
        // rather than trusting the source.
        if names.len() > MAX_TREE_CHILDREN {
            names.truncate(MAX_TREE_CHILDREN);
            self.truncated.insert(path.to_owned());
        }
        for name in &names {
            self.resolve_node(view, &child_path(path, name));
        }
        self.children.insert(path.to_owned(), names);
    }

    fn resolve_value(&mut self, view: &dyn MemoryView, path: &str) {
        if self.values.contains_key(path) {
            return;
        }
        let value = match view.read(path) {
            Ok(Some(value)) => value,
            Ok(None) => Value::Null,
            Err(error) => Value::String(error),
        };
        self.values.insert(path.to_owned(), value);
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((parent, _)) => parent.into(),
    }
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
    nodes: MemoryTree,
    expanded: BTreeSet<String>,
    memory_viewport_height: usize,
    memory_window_start: usize,
    panel_bounds: PanelBounds,
    composer: TextInputState,
    transcript_scroll: ScrollState,
    preview_scroll: ScrollState,
    preview_content_height: usize,
    preview_cache: Option<PreviewCache>,
    rows: Vec<TreeRow>,
    /// A row path to reselect once the tree resolves it again.
    pending_selection: Option<String>,
    timeline: Vec<TimelineEntry>,
    working: bool,
    failure: Option<String>,
    version: u64,
    model: String,
}

impl App {
    fn new(version: u64, model: String) -> Self {
        let mut preview_scroll = ScrollState::new();
        preview_scroll.jump_to_top();
        let expanded = BTreeSet::from(["/".into()]);
        let nodes = MemoryTree::default();
        let rows = tree_rows(&expanded, &nodes);
        Self {
            focus: Focus::Composer,
            tree: SelectState::new(),
            nodes,
            expanded,
            memory_viewport_height: 1,
            memory_window_start: 0,
            panel_bounds: PanelBounds::default(),
            composer: TextInputState::new(),
            transcript_scroll: ScrollState::new(),
            preview_scroll,
            preview_content_height: 1,
            preview_cache: None,
            rows,
            pending_selection: None,
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
        self.nodes
            .value(&self.selected().path)
            .unwrap_or(&PENDING_VALUE)
    }

    /// Resolves what the current tree and selection need, and nothing else.
    ///
    /// Runs once per frame. Every node goes through the same three process
    /// operations, so a mounted folder and committed memory are browsed the
    /// same way.
    fn resolve(&mut self, view: &dyn MemoryView) {
        let before = self.nodes.resolved();
        self.nodes.resolve_node(view, "/");
        // Expanded paths iterate parent before child, so a directory's kind is
        // known by the time its own listing is requested.
        for path in self.expanded.clone() {
            self.nodes.resolve_children(view, &path);
        }
        self.rows = tree_rows(&self.expanded, &self.nodes);
        // A commit discards every resolved node, so the row the operator was
        // reading is restored once its ancestors are listed again.
        if let Some(path) = self.pending_selection.take() {
            self.tree
                .select(Some(self.visible_index_or_ancestor(&path)));
            self.keep_tree_selection_visible();
        }

        // A row needs a value only to summarize an array item; the selected
        // row needs one for its preview. Content is read eagerly only where
        // the process reports it is already resident.
        let summarized = self
            .rows
            .iter()
            .filter(|row| {
                self.nodes
                    .node(&row.path)
                    .is_some_and(|node| node.resident())
                    && self
                        .nodes
                        .node(&parent_path(&row.path))
                        .is_some_and(Node::array)
            })
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        for path in summarized {
            self.nodes.resolve_value(view, &path);
        }
        let selected = self.selected().path.clone();
        self.nodes.resolve_value(view, &selected);

        if before != self.nodes.resolved() {
            self.preview_cache = None;
            self.rebuild_tree(&selected);
        }
    }

    /// Applies one committed change, dropping only what it invalidated.
    ///
    /// The process reports the paths a transition touched, so an unrelated
    /// commit no longer costs a re-walk of every open directory. Nodes the
    /// change did not name stay resolved — including mount nodes, which no
    /// event can report an external change for.
    fn refresh_process(&mut self, change: &Change) {
        // Several commits can arrive between frames. After the first
        // invalidation, the selected row may only be a temporary ancestor, so
        // retain the original path until resolution restores it.
        if self.pending_selection.is_none() {
            self.pending_selection = Some(self.selected().path.clone());
        }
        if change.paths().is_empty() {
            self.nodes.clear();
        } else {
            self.nodes.invalidate(change);
        }
        self.rows = tree_rows(&self.expanded, &self.nodes);
        self.preview_cache = None;
        self.version = change.version();
    }

    /// Drops every resolved node so the next frame reads the tree again.
    fn reload(&mut self) {
        self.pending_selection = Some(self.selected().path.clone());
        self.nodes.clear();
        self.rows = tree_rows(&self.expanded, &self.nodes);
        self.preview_cache = None;
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
        self.rows = tree_rows(&self.expanded, &self.nodes);
        self.tree
            .select(Some(self.visible_index_or_ancestor(selected_path)));
        self.keep_tree_selection_visible();
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
        if path == "/" {
            return;
        }
        let parent = parent_path(&path);
        if let Some(index) = self.rows.iter().position(|row| row.path == parent) {
            self.tree.select(Some(index));
            self.keep_tree_selection_visible();
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
        self.keep_tree_selection_visible();
    }

    fn memory_window(&self) -> VirtualWindow {
        VirtualWindow::new(
            self.rows.len(),
            self.memory_viewport_height,
            self.memory_window_start,
        )
    }

    fn keep_tree_selection_visible(&mut self) {
        let visible = self.memory_viewport_height.max(1).min(self.rows.len());
        let selected = self
            .tree
            .selected()
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        let mut start = self
            .memory_window_start
            .min(VirtualWindow::max_start_for(self.rows.len(), visible));
        if selected < start {
            start = selected;
        } else if selected >= start.saturating_add(visible) {
            start = selected.saturating_add(1).saturating_sub(visible);
        }
        self.memory_window_start = start;
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
                if target == Focus::Memory {
                    let selected = self.tree.selected();
                    let first_visible = self.memory_window().start();
                    let _ = self.tree.handle_mouse(
                        event,
                        self.rows.len(),
                        panel_body(self.panel_bounds.memory),
                        first_visible,
                    );
                    if self.tree.selected() != selected {
                        self.preview_scroll.jump_to_top();
                    }
                    self.keep_tree_selection_visible();
                }
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
                code: KeyCode::BackTab,
                ..
            })
        ) {
            self.focus = match self.focus {
                Focus::Composer => Focus::Preview,
                Focus::Memory => Focus::Composer,
                Focus::Preview => Focus::Memory,
            };
            return AppAction::Continue;
        }

        if matches!(
            event,
            Event::Key(Key {
                code: KeyCode::Tab,
                ctrl: false,
                alt: false,
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
                    // Nothing reports an external change to a mounted source,
                    // so re-reading the tree stays a deliberate action.
                    Event::Key(Key {
                        code: KeyCode::Char('r'),
                        ..
                    }) => self.reload(),
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
                self.keep_tree_selection_visible();
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

pub async fn run(mut svit: Svit, model: String) -> Result<(), String> {
    let mut app = App::new(MemoryView::version(&svit), model);
    let inbox = svit.inbox();
    let mut events = svit.events();
    let mut outbox = svit.outbox();
    svit.start().map_err(|error| error.to_string())?;

    let ui_result = run_terminal(&mut app, &svit, &inbox, &mut events, &mut outbox).await;
    svit.block().await.map_err(|error| error.to_string())?;
    ui_result
}

async fn run_terminal(
    app: &mut App,
    svit: &Svit,
    inbox: &Inbox,
    events: &mut Events,
    outbox: &mut Outbox,
) -> Result<(), String> {
    let theme = lampa_theme();
    let probes = UiProbes::default();
    let _session = TerminalSession::enter().map_err(|error| error.to_string())?;
    let mut terminal = Terminal::with_options(
        HyperlinkBackend::new(io::stdout(), true),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .map_err(|error| error.to_string())?;

    let result = async {
        loop {
            while let Ok(event) = events.try_recv() {
                match event {
                    SvitEvent::Committed(change) => app.refresh_process(&change),
                    SvitEvent::Failed(error) => app.push_error(error),
                }
            }
            while let Ok(message) = outbox.try_recv() {
                app.push_assistant(message);
            }
            // The tree resolves here, once per frame, so the console fetches
            // only the nodes the visible tree and current selection need.
            app.resolve(svit);

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
                            .await
                            .map_err(|error| error.to_string())?;
                        app.push_user(text);
                    }
                    AppAction::Quit => break,
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = terminal.clear();
    result
}

fn tree_rows(expanded: &BTreeSet<String>, nodes: &MemoryTree) -> Vec<TreeRow> {
    let expandable = nodes.is_directory("/");
    let root_marker = match (expandable, expanded.contains("/")) {
        (true, true) => "▾ ",
        (true, false) => "▸ ",
        (false, _) => "  ",
    };
    let mut rows = vec![TreeRow {
        label: format!("{root_marker}/"),
        path: "/".into(),
        expandable,
    }];
    if expanded.contains("/") {
        flatten_tree("/", "", true, expanded, nodes, &mut rows);
    }
    rows
}

/// Labels one child row from its own facts and, for array items, its value.
fn tree_label(parent: &str, name: &str, path: &str, nodes: &MemoryTree) -> String {
    if nodes.node(parent).is_some_and(Node::array) {
        let summary = nodes
            .value(path)
            .map(tree_item_summary)
            .unwrap_or_else(|| "…".into());
        return format!("[{name}] - {summary}");
    }
    // A node whose content lives outside the process announces where reading
    // it will go, so the cost of expanding a row is visible before opening it.
    match nodes.node(path) {
        Some(node) if !node.resident() => format!("{name} ({})", node.locality),
        _ => name.to_owned(),
    }
}

fn flatten_tree(
    path: &str,
    indent: &str,
    last_parent: bool,
    expanded: &BTreeSet<String>,
    nodes: &MemoryTree,
    rows: &mut Vec<TreeRow>,
) {
    let mut children = nodes
        .children(path)
        .iter()
        .map(|name| {
            let child = child_path(path, name);
            let label = tree_label(path, name, &child, nodes);
            let expandable = nodes.is_directory(&child);
            (label, child, expandable)
        })
        .collect::<Vec<_>>();
    if nodes.truncated.contains(path) {
        children.push((
            format!("… first {MAX_TREE_CHILDREN} entries"),
            child_path(path, "…"),
            false,
        ));
    }

    let next_indent = format!("{indent}{}", if last_parent { "  " } else { "│ " });
    let child_count = children.len();
    for (index, (label, child, expandable)) in children.into_iter().enumerate() {
        let last = index + 1 == child_count;
        let branch = if last { "└─" } else { "├─" };
        let marker = match (expandable, expanded.contains(&child)) {
            (true, true) => "▾ ",
            (true, false) => "▸ ",
            (false, _) => "  ",
        };
        rows.push(TreeRow {
            label: format!("{next_indent}{branch}{marker}{label}"),
            path: child.clone(),
            expandable,
        });
        if expanded.contains(&child) {
            flatten_tree(&child, &next_indent, last, expanded, nodes, rows);
        }
    }
}

fn tree_item_summary(value: &Value) -> String {
    match value {
        Value::String(text) => bounded_tree_text(text),
        Value::Array(values) => item_count("array", values.len()),
        Value::Map(values) => {
            const IDENTITY_KEYS: [&str; 6] = ["name", "title", "label", "operation", "type", "id"];
            IDENTITY_KEYS
                .iter()
                .find_map(|key| {
                    values
                        .get(*key)
                        .and_then(tree_scalar_summary)
                        .map(|value| format!("{key}: {value}"))
                })
                .or_else(|| {
                    values.iter().find_map(|(key, value)| {
                        tree_scalar_summary(value).map(|value| format!("{key}: {value}"))
                    })
                })
                .unwrap_or_else(|| item_count("object", values.len()))
        }
        Value::Script(_) => "script".into(),
        value => tree_scalar_summary(value).expect("non-container value is scalar"),
    }
}

fn tree_scalar_summary(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(bounded_tree_text(text)),
        Value::Null | Value::Bool(_) | Value::Integer(_) | Value::Number(_) => Some(
            serde_json::to_string(&value.to_json())
                .expect("persistent scalar values always serialize to JSON"),
        ),
        Value::Array(_) | Value::Map(_) | Value::Script(_) => None,
    }
}

fn bounded_tree_text(text: &str) -> String {
    let mut summary = String::new();
    let mut pending_space = false;
    let mut truncated = false;
    for character in text.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !summary.is_empty();
            continue;
        }
        let extra = character.len_utf8() + usize::from(pending_space);
        if summary.len() + extra > MAX_TREE_ITEM_PREVIEW_BYTES {
            truncated = true;
            break;
        }
        if pending_space {
            summary.push(' ');
            pending_space = false;
        }
        summary.push(character);
    }
    if truncated {
        summary.push('…');
    }
    if summary.is_empty() {
        "\"\"".into()
    } else {
        summary
    }
}

fn item_count(kind: &str, count: usize) -> String {
    format!("{kind} · {count} item{}", if count == 1 { "" } else { "s" })
}

fn build_view(app: &mut App, area: Rect, theme: &Theme, probes: &UiProbes) -> Element {
    let memory_width = MEMORY_WIDTH.min(area.width.saturating_sub(48)).max(20);
    let preview_width = area.width.saturating_sub(memory_width) / 3;
    let conversation_width = area
        .width
        .saturating_sub(memory_width)
        .saturating_sub(preview_width);
    let conversation_content_width = conversation_width.saturating_sub(4).max(1);
    app.memory_viewport_height = usize::from(area.height.saturating_sub(3)).max(1);
    app.keep_tree_selection_visible();
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
            probes.conversation.wrap(conversation_view(
                app,
                theme,
                &probes.composer,
                conversation_content_width,
            )),
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
    let window = app.memory_window();
    let lines = window
        .range()
        .map(|index| &app.rows[index])
        .map(|row| Line::from(row.label.clone()))
        .collect();
    let border = if app.focus == Focus::Memory {
        theme.border_focused
    } else {
        theme.border
    };
    element(
        Boxed::new(element(
            SelectList::windowed(lines, window, &app.tree).scrollbar(true),
        ))
        .title(" Memory ")
        .border_color(border)
        .padding(Padding::symmetric(0, 0)),
    )
}

fn conversation_view(
    app: &mut App,
    theme: &Theme,
    probe: &RectProbe,
    content_width: u16,
) -> Element {
    let lines = timeline_lines(&app.timeline, content_width, theme);
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
        Value::Map(_) => PreviewDocument {
            format: PreviewFormat::Summary,
            source: container_summary(value),
        },
        Value::Array(_) => PreviewDocument {
            format: PreviewFormat::Summary,
            source: container_summary(value),
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

fn container_summary(value: &Value) -> String {
    let mut summary = value_summary(value);
    let mut remaining = MAX_PREVIEW_ITEMS;
    append_container_children(&mut summary, value, "", MAX_PREVIEW_DEPTH, &mut remaining);
    summary
}

fn append_container_children(
    summary: &mut String,
    value: &Value,
    indent: &str,
    depth: usize,
    remaining: &mut usize,
) {
    if depth == 0 {
        return;
    }
    let count = match value {
        Value::Map(values) => values.len(),
        Value::Array(values) => values.len(),
        _ => return,
    };
    let mut shown = 0;
    match value {
        Value::Map(values) => {
            for (name, child) in values {
                if !append_container_child(summary, name, child, indent, depth, remaining) {
                    break;
                }
                shown += 1;
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                if !append_container_child(
                    summary,
                    &index.to_string(),
                    child,
                    indent,
                    depth,
                    remaining,
                ) {
                    break;
                }
                shown += 1;
            }
        }
        _ => unreachable!("container kind checked above"),
    }
    if shown < count {
        summary.push_str(&format!(
            "\n{indent}… {} more items; expand the memory tree to inspect them …",
            count - shown
        ));
    }
}

fn append_container_child(
    summary: &mut String,
    name: &str,
    value: &Value,
    indent: &str,
    depth: usize,
    remaining: &mut usize,
) -> bool {
    if *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    summary.push_str(&format!("\n{indent}{name}  {}", value_summary(value)));
    if depth > 1 && matches!(value, Value::Map(_) | Value::Array(_)) {
        append_container_children(summary, value, &format!("{indent}  "), depth - 1, remaining);
    }
    true
}

fn value_summary(value: &Value) -> String {
    match value {
        Value::String(text) => inline_string_summary(text),
        Value::Array(values) => format!("array · {} items", values.len()),
        Value::Map(values) => format!("object · {} items", values.len()),
        Value::Script(_) => "script".into(),
        value => serde_json::to_string(&value.to_json())
            .expect("persistent scalar values always serialize to JSON"),
    }
}

fn inline_string_summary(text: &str) -> String {
    let mut end = text.len().min(MAX_INLINE_VALUE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if end < text.len() { "…" } else { "" };
    serde_json::to_string(&format!("{}{suffix}", &text[..end]))
        .expect("Rust strings always serialize to JSON")
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

fn timeline_lines(
    entries: &[TimelineEntry],
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
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
                            lines.extend(tuika::components::markdown::to_lines(
                                &part.text,
                                content_width,
                                theme,
                                &StyleSheet::from_theme(theme),
                                CodeHighlighter::With(&PreviewHighlighter::default()),
                            ));
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
    use svit::{LLMSIM_MODEL_ID, LlmSimConfig, Mount, Reasoner, llm_sim_provider, value};
    use tuika::testing::{grid, render};

    /// A [`MemoryView`] over one in-memory value.
    ///
    /// It answers `discover`, `stat`, and `read` exactly as a process does, so
    /// the console under test browses the same node interface it does live.
    struct ValueView {
        root: Value,
        version: u64,
        /// Paths whose locality the process would report as non-resident.
        localities: BTreeMap<String, String>,
    }

    impl ValueView {
        fn new(root: Value) -> Self {
            Self {
                root,
                version: 0,
                localities: BTreeMap::new(),
            }
        }

        fn locality(mut self, path: &str, locality: &str) -> Self {
            self.localities.insert(path.to_owned(), locality.to_owned());
            self
        }

        fn at(&self, path: &str) -> Option<&Value> {
            if path == "/" {
                return Some(&self.root);
            }
            let mut current = &self.root;
            for segment in path.trim_start_matches('/').split('/') {
                current = match current {
                    Value::Map(values) => values.get(segment)?,
                    Value::Array(values) => values.get(segment.parse::<usize>().ok()?)?,
                    _ => return None,
                };
            }
            Some(current)
        }
    }

    impl MemoryView for ValueView {
        fn discover(&self, path: &str) -> Result<Vec<String>, String> {
            match self.at(path) {
                Some(Value::Map(values)) => Ok(values.keys().cloned().collect()),
                Some(Value::Array(values)) => {
                    Ok((0..values.len()).map(|index| index.to_string()).collect())
                }
                _ => Err(format!("invalid state path: {path}")),
            }
        }

        fn stat(&self, path: &str) -> Result<Option<Value>, String> {
            let Some(value) = self.at(path) else {
                return Ok(None);
            };
            let (kind, content) = match value {
                Value::Map(_) => ("directory", "object"),
                Value::Array(_) => ("directory", "array"),
                Value::String(_) => ("leaf", "text/plain"),
                Value::Script(_) => ("leaf", "svit-script"),
                _ => ("leaf", "scalar"),
            };
            let locality = self
                .localities
                .get(path)
                .cloned()
                .unwrap_or_else(|| "cache".into());
            Ok(Some(value!({
                "kind": kind,
                "facts": {"content": content},
                "locality": locality,
                "path": path
            })))
        }

        fn read(&self, path: &str) -> Result<Option<Value>, String> {
            Ok(self.at(path).cloned())
        }

        fn version(&self) -> u64 {
            self.version
        }
    }

    /// One console plus the view it browses.
    struct TestApp {
        app: App,
        view: ValueView,
    }

    impl std::ops::Deref for TestApp {
        type Target = App;

        fn deref(&self) -> &App {
            &self.app
        }
    }

    impl std::ops::DerefMut for TestApp {
        fn deref_mut(&mut self) -> &mut App {
            &mut self.app
        }
    }

    impl TestApp {
        fn handle(&mut self, event: &Event) -> AppAction {
            let action = self.app.handle(event);
            self.app.resolve(&self.view);
            action
        }

        /// Selects one row and resolves what the new selection needs, the way
        /// the frame loop does.
        fn select(&mut self, path: &str) {
            let index = self
                .app
                .rows
                .iter()
                .position(|row| row.path == path)
                .unwrap_or_else(|| panic!("no row for {path}"));
            self.app.tree.select(Some(index));
            self.app.resolve(&self.view);
        }

        fn refresh_process(&mut self, root: Value, version: u64) {
            let changed = vec!["/".to_owned()];
            self.view.root = root;
            self.view.version = version;
            self.app
                .refresh_process(&Change::notification(version, changed));
            self.app.resolve(&self.view);
        }
    }

    fn test_app(root: Value, version: u64) -> TestApp {
        test_app_with(ValueView::new(root), version, "test")
    }

    fn test_app_with(view: ValueView, version: u64, model: &str) -> TestApp {
        let mut app = App::new(version, model.into());
        app.resolve(&view);
        TestApp { app, view }
    }

    fn layout(app: &mut TestApp, width: u16, height: u16) -> UiProbes {
        app.app.resolve(&app.view);
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(
            &mut app.app,
            Rect::new(0, 0, width, height),
            &theme,
            &probes,
        );
        let _ = render(root.as_ref(), width, height, &theme);
        app.app.capture_panel_bounds(&probes);
        probes
    }

    fn panel_mouse(kind: MouseKind, rect: Rect) -> Event {
        Event::Mouse(Mouse::at(
            kind,
            rect.x + rect.width / 2,
            rect.y + rect.height / 2,
        ))
    }

    fn expand_paths(app: &mut TestApp, paths: &[&str]) {
        let selected_path = app.selected().path.clone();
        app.expanded
            .extend(paths.iter().map(|path| (*path).to_owned()));
        app.app.rebuild_tree(&selected_path);
        app.app.resolve(&app.view);
    }

    /// Drives the console against a real process, not the in-memory double.
    ///
    /// Everything else here exercises `MemoryView` through `ValueView`; this
    /// covers the implementation Lampa actually runs against, including a real
    /// folder mount resolved from disk.
    #[tokio::test]
    async fn the_console_browses_a_real_process_and_its_mounts() {
        let folder = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svit/examples/mount-data");
        let svit = Svit::builder("svit://local/lampa/smoke")
            .unwrap()
            .reasoner(Reasoner::new(
                LLMSIM_MODEL_ID,
                llm_sim_provider(LlmSimConfig::scripted(Vec::new())),
            ))
            .memory("profile", value!({"name": "Ada"}))
            .mount("files", Mount::folder(&folder).unwrap())
            .build()
            .await
            .unwrap();

        let mut app = App::new(MemoryView::version(&svit), "test".into());
        app.resolve(&svit);

        // The committed namespace resolves through the same interface.
        let paths = app
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"/memory"));
        assert!(paths.contains(&"/mounts"));
        assert!(paths.contains(&"/system"));

        app.expanded.insert("/thread".into());
        app.expanded.insert("/thread/events".into());
        app.resolve(&svit);
        let event_label = &app
            .rows
            .iter()
            .find(|row| row.path == "/thread/events/0")
            .expect("Svit initialization projects a session event")
            .label;
        assert!(event_label.contains("session.started"), "{event_label}");

        app.expanded.insert("/mounts".into());
        app.expanded.insert("/mounts/files".into());
        app.expanded.insert("/mounts/files/notes".into());
        app.resolve(&svit);

        // A real directory listed from disk, one node at a time.
        assert_eq!(
            app.nodes.children("/mounts/files"),
            ["greeting.txt".to_owned(), "notes".to_owned()]
        );
        assert!(app.nodes.is_directory("/mounts/files/notes"));
        assert!(!app.nodes.is_directory("/mounts/files/greeting.txt"));

        // A local mount is labelled with its cost and not read to render.
        let label = app
            .rows
            .iter()
            .find(|row| row.path == "/mounts/files/greeting.txt")
            .unwrap()
            .label
            .clone();
        assert!(label.contains("greeting.txt (local)"), "{label}");
        assert_eq!(app.nodes.value("/mounts/files/greeting.txt"), None);

        // Selecting it reads the real file.
        let index = app
            .rows
            .iter()
            .position(|row| row.path == "/mounts/files/greeting.txt")
            .unwrap();
        app.tree.select(Some(index));
        app.resolve(&svit);
        assert_eq!(
            app.selected_value(),
            &Value::from("hello from a real folder\n")
        );

        // The frame renders without panicking and shows the mounted file.
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(&mut app, Rect::new(0, 0, 160, 40), &theme, &probes);
        let frame = grid(&render(root.as_ref(), 160, 40, &theme));
        assert!(frame.contains("files"), "{frame}");
        assert!(frame.contains("greeting.txt"), "{frame}");
        assert!(frame.contains("hello from a real"), "{frame}");
    }

    #[tokio::test]
    async fn a_real_commit_invalidates_only_what_it_changed() {
        let folder = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svit/examples/mount-data");
        let mut svit = Svit::builder("svit://local/lampa/smoke-commit")
            .unwrap()
            .reasoner(Reasoner::new(
                LLMSIM_MODEL_ID,
                llm_sim_provider(LlmSimConfig::scripted(Vec::new())),
            ))
            .memory("count", value!(0))
            .mount("files", Mount::folder(&folder).unwrap())
            .build()
            .await
            .unwrap();
        let mut events = svit.events();

        let mut app = App::new(MemoryView::version(&svit), "test".into());
        app.expanded.insert("/mounts".into());
        app.expanded.insert("/mounts/files".into());
        app.resolve(&svit);
        assert_eq!(app.nodes.children("/mounts/files").len(), 2);

        // Building a Svit commits its thread and built-in catalog, so the
        // write continues an existing version chain rather than starting one.
        let committed_before = MemoryView::version(&svit);
        let change = svit.write("/memory/count", value!(1)).await.unwrap();
        assert_eq!(change.paths(), ["/memory/count".to_owned()]);
        assert_eq!(change.version(), committed_before + 1);

        let SvitEvent::Committed(published) = events.try_recv().unwrap() else {
            panic!("a committed write publishes a change");
        };
        app.refresh_process(&published);

        // The mounted directory the operator has open survives the commit.
        assert_eq!(app.nodes.children("/mounts/files").len(), 2);
        assert!(!app.nodes.children.contains_key("/memory"));

        app.resolve(&svit);
        assert_eq!(app.version, change.version());
    }

    #[test]
    fn the_console_holds_no_node_it_has_not_resolved() {
        let view = ValueView::new(value!({
            "memory": {"notes": {"a": 1}},
            "mounts": {"cwd": {"README.md": "hello"}}
        }));
        let mut app = App::new(0, "test".into());

        // Before resolving anything the console knows only that a root exists.
        assert_eq!(
            app.rows
                .iter()
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            ["/"]
        );

        app.resolve(&view);

        // Resolving the open root lists its children and nothing deeper.
        let paths = app
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["/", "/memory", "/mounts"]);

        app.expanded.insert("/mounts".into());
        app.expanded.insert("/mounts/cwd".into());
        app.resolve(&view);

        // A mount is opened through the same discover/stat path as memory.
        let paths = app
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "/",
                "/memory",
                "/mounts",
                "/mounts/cwd",
                "/mounts/cwd/README.md"
            ]
        );
        assert!(app.nodes.is_directory("/mounts/cwd"));
        assert!(!app.nodes.is_directory("/mounts/cwd/README.md"));
    }

    #[test]
    fn a_non_resident_row_announces_where_reading_it_will_go() {
        let view = ValueView::new(value!({"mounts": {"cwd": {"README.md": "hello"}}}))
            .locality("/mounts/cwd", "local")
            .locality("/mounts/cwd/README.md", "local");
        let mut app = test_app_with(view, 0, "test");
        expand_paths(&mut app, &["/mounts", "/mounts/cwd"]);

        let label = |path: &str| {
            app.rows
                .iter()
                .find(|row| row.path == path)
                .unwrap()
                .label
                .clone()
        };

        assert!(label("/mounts/cwd").contains("cwd (local)"));
        assert!(label("/mounts/cwd/README.md").contains("README.md (local)"));
        // Resident committed state carries no cost annotation.
        assert!(!label("/mounts").contains('('));
        // A non-resident leaf is not read just to render its row.
        assert_eq!(app.nodes.value("/mounts/cwd/README.md"), None);
    }

    #[test]
    fn a_commit_rereads_the_tree_without_losing_the_selected_row() {
        let mut app = test_app(
            value!({"memory": {"profile": {"name": "Ada"}}, "mounts": {"cwd": {"a.txt": "one"}}}),
            1,
        );
        expand_paths(&mut app, &["/mounts", "/mounts/cwd"]);
        app.select("/mounts/cwd/a.txt");

        // A commit invalidates everything resolved from the previous version,
        // including external nodes that may have changed underneath.
        app.refresh_process(
            value!({"memory": {"profile": {"name": "Ada"}}, "mounts": {"cwd": {"a.txt": "two"}}}),
            2,
        );

        assert_eq!(app.version, 2);
        assert_eq!(app.selected().path, "/mounts/cwd/a.txt");
        assert_eq!(app.selected_value(), &Value::from("two"));
    }

    #[test]
    fn an_unrelated_commit_keeps_the_rest_of_the_tree_resolved() {
        let mut app = test_app(
            value!({
                "memory": {"count": 1},
                "mounts": {"cwd": {"a.txt": "one", "b.txt": "two"}}
            }),
            1,
        );
        expand_paths(&mut app, &["/memory", "/mounts", "/mounts/cwd"]);
        app.select("/mounts/cwd/a.txt");
        let resolved_before = app.nodes.resolved();

        // A commit that names only /memory/count must not cost a re-walk of
        // the mounted directory the operator has open.
        app.view.root = value!({
            "memory": {"count": 2},
            "mounts": {"cwd": {"a.txt": "one", "b.txt": "two"}}
        });
        app.app
            .refresh_process(&Change::notification(2, vec!["/memory/count".to_owned()]));

        assert_eq!(app.nodes.children("/mounts/cwd").len(), 2);
        assert_eq!(
            app.nodes.value("/mounts/cwd/a.txt"),
            Some(&Value::from("one"))
        );
        // The changed path and its parent listing are the only casualties.
        assert!(app.nodes.node("/memory/count").is_none());
        assert!(!app.nodes.children.contains_key("/memory"));
        assert!(app.nodes.resolved() < resolved_before);

        app.app.resolve(&app.view);
        app.select("/memory/count");

        assert_eq!(app.version, 2);
        assert_eq!(app.selected_value(), &Value::from(2));
    }

    #[test]
    fn a_commit_that_writes_a_mount_invalidates_that_node() {
        let mut app = test_app(value!({"mounts": {"cwd": {"a.txt": "one"}}}), 1);
        expand_paths(&mut app, &["/mounts", "/mounts/cwd"]);
        app.select("/mounts/cwd/a.txt");

        // A granted mount write is reported like any other changed path.
        app.view.root = value!({"mounts": {"cwd": {"a.txt": "rewritten"}}});
        app.app.refresh_process(&Change::notification(
            2,
            vec!["/mounts/cwd/a.txt".to_owned()],
        ));
        app.app.resolve(&app.view);

        assert_eq!(app.selected_value(), &Value::from("rewritten"));
    }

    #[test]
    fn reloading_rereads_a_tree_no_event_could_report() {
        let mut app = test_app(value!({"mounts": {"cwd": {"a.txt": "one"}}}), 1);
        expand_paths(&mut app, &["/mounts", "/mounts/cwd"]);
        app.select("/mounts/cwd/a.txt");

        // An external edit produces no event, so the console keeps showing the
        // value it last read until the operator asks for a reload.
        app.view.root = value!({"mounts": {"cwd": {"a.txt": "changed on disk"}}});
        app.app.resolve(&app.view);
        assert_eq!(app.selected_value(), &Value::from("one"));

        app.focus = Focus::Memory;
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Char('r'))));

        assert_eq!(app.selected_value(), &Value::from("changed on disk"));
        assert_eq!(app.selected().path, "/mounts/cwd/a.txt");
    }

    #[test]
    fn a_truncated_listing_says_so_instead_of_looking_complete() {
        let entries = (0..MAX_TREE_CHILDREN + 5)
            .map(|index| (format!("file-{index:04}"), Value::from("x")))
            .collect::<std::collections::BTreeMap<_, _>>();
        let view = ValueView::new(Value::Map(std::collections::BTreeMap::from([(
            "memory".to_owned(),
            Value::Map(entries),
        )])));
        let mut app = test_app_with(view, 0, "test");

        expand_paths(&mut app, &["/memory"]);

        assert_eq!(
            app.rows
                .iter()
                .filter(|row| row.path.starts_with("/memory/file-"))
                .count(),
            MAX_TREE_CHILDREN
        );
        assert!(
            app.rows
                .iter()
                .any(|row| row.label.contains("first 200 entries"))
        );
    }

    #[test]
    fn initial_tree_opens_only_the_root() {
        let app = test_app(
            value!({
                "thread": {"events": []},
                "memory": {"profile": {"name": "Ada"}}
            }),
            0,
        );

        assert_eq!(app.expanded, BTreeSet::from(["/".into()]));
        assert!(app.rows.iter().any(|row| row.path == "/memory"));
        assert!(!app.rows.iter().any(|row| row.path == "/memory/profile"));
    }

    #[test]
    fn array_tree_rows_include_bounded_item_previews() {
        let long_text = format!(
            "start\n\u{1b}{}",
            "x".repeat(MAX_TREE_ITEM_PREVIEW_BYTES * 2)
        );
        let root = value!({
            "items": [
                "plain text",
                7,
                {"id": "secondary", "name": "Ada"},
                {"count": 2},
                {"nested": {"value": "hidden"}},
                long_text
            ]
        });
        let mut app = test_app(root, 0);
        let all = app
            .rows
            .iter()
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        expand_paths(
            &mut app,
            &all.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        expand_paths(&mut app, &["/items"]);
        let label = |path: &str| {
            app.rows
                .iter()
                .find(|row| row.path == path)
                .unwrap()
                .label
                .as_str()
        };

        assert!(label("/items/0").ends_with("[0] - plain text"));
        assert!(label("/items/1").ends_with("[1] - 7"));
        assert!(label("/items/2").ends_with("[2] - name: Ada"));
        assert!(label("/items/3").ends_with("[3] - count: 2"));
        assert!(label("/items/4").ends_with("[4] - object · 1 item"));
        assert!(label("/items/5").contains("[5] - start "));
        assert!(label("/items/5").ends_with('…'));
        assert!(!label("/items/5").contains('\n'));
        assert!(!label("/items/5").contains('\u{1b}'));
    }

    #[test]
    fn tree_flattens_complete_process_memory_and_preview_tracks_selection() {
        let mut app = test_app_with(
            ValueView::new(value!({
                "thread": {"messages": [], "events": []},
                "memory": {"profile": {"name": "Ada"}, "scores": [3, 5]}
            })),
            7,
            "test-model",
        );

        assert_eq!(app.rows[0].label, "▾ /");
        assert!(app.rows.iter().any(|row| row.path == "/memory"));
        assert!(!app.rows.iter().any(|row| row.path == "/thread/messages"));
        expand_paths(
            &mut app,
            &["/thread", "/memory", "/memory/profile", "/memory/scores"],
        );
        assert!(app.rows.iter().any(|row| row.path == "/thread/messages"));
        assert!(app.rows.iter().any(|row| row.label.ends_with("name")));
        app.select("/memory/profile/name");

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
            "/thread/system_prompt",
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
    fn assistant_timeline_renders_markdown_and_styles_bare_urls() {
        let theme = lampa_theme();
        let entries = vec![TimelineEntry::Message(Box::new(Message::assistant(
            "**Everruns**: https://everruns.com",
        )))];
        let lines = timeline_lines(&entries, 80, &theme);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let url = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("https://everruns.com"))
            .unwrap();
        let emphasized = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == "Everruns")
            .unwrap();

        assert!(rendered.contains("Everruns"));
        assert!(!rendered.contains("**"));
        assert!(emphasized.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(url.style.fg, Some(theme.code.link));
        assert!(url.style.add_modifier.contains(Modifier::UNDERLINED));
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
    fn container_preview_shows_two_descendant_levels() {
        let theme = lampa_theme();
        let value = value!({
            "profile": {
                "biography": "Ada",
                "details": {"private": "one level too deep"}
            },
            "scores": [3, 5]
        });
        let rendered = preview_lines("/memory", &value, 60, &theme)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("object · 2 items"));
        assert!(rendered.contains("profile  object · 2 items"));
        assert!(rendered.contains("biography  \"Ada\""));
        assert!(rendered.contains("details  object · 1 items"));
        assert!(rendered.contains("scores  array · 2 items"));
        assert!(rendered.contains("0  3"));
        assert!(rendered.contains("1  5"));
        assert!(!rendered.contains("one level too deep"));
    }

    #[test]
    fn container_preview_shows_scalar_child_values() {
        let theme = lampa_theme();
        let event = value!({
            "context": {},
            "data": {"private": "nested value"},
            "id": "event-123",
            "sequence": 7,
            "session_id": "session-456",
            "ts": "2026-08-11T12:34:56Z",
            "type": "response.completed"
        });
        let rendered = preview_lines("/thread/events/0", &event, 80, &theme)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("context  object"));
        assert!(rendered.contains("data  object"));
        assert!(rendered.contains("private  \"nested value\""));
        assert!(rendered.contains("id  \"event-123\""));
        assert!(rendered.contains("sequence  7"));
        assert!(rendered.contains("session_id  \"session-456\""));
        assert!(rendered.contains("type  \"response.completed\""));
    }

    #[test]
    fn container_preview_bounds_inline_string_values() {
        let theme = lampa_theme();
        let long_value = format!("start-{}-end", "x".repeat(MAX_INLINE_VALUE_BYTES * 2));
        let value = value!({"field": long_value});
        let rendered = preview_lines("/memory", &value, 80, &theme)
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("field  \"start-"));
        assert!(rendered.contains('…'));
        assert!(!rendered.contains("-end"));
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
        let mut app = test_app(
            value!({"thread": {}, "memory": {"profile": {"name": "Ada"}}}),
            1,
        );
        expand_paths(&mut app, &["/memory"]);
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/profile")
            .unwrap();
        app.tree.select(Some(selected));

        app.refresh_process(
            value!({
                "thread": {"messages": []},
                "memory": {"profile": {"name": "Grace"}, "z": true}
            }),
            2,
        );

        assert_eq!(app.selected().path, "/memory/profile");
        assert_eq!(app.version, 2);
    }

    #[test]
    fn batched_commit_refreshes_preserve_the_original_selection() {
        let mut app = test_app(
            value!({
                "inbox": [],
                "memory": {"profile": {"name": "Ada"}},
                "thread": {"events": []}
            }),
            1,
        );
        expand_paths(&mut app, &["/memory", "/memory/profile"]);
        app.select("/memory/profile/name");

        app.app
            .refresh_process(&Change::notification(2, vec!["/inbox".to_owned()]));
        app.app
            .refresh_process(&Change::notification(3, vec!["/thread/events".to_owned()]));
        app.app.resolve(&app.view);

        assert_eq!(app.selected().path, "/memory/profile/name");
        assert!(app.expanded.contains("/memory"));
        assert!(app.expanded.contains("/memory/profile"));
    }

    #[test]
    fn composer_submission_is_trimmed_and_cleared() {
        let mut app = test_app(value!({}), 0);
        app.focus = Focus::Composer;
        app.composer.set_text("  remember blue  ");

        let action = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert_eq!(action, AppAction::Submit("remember blue".into()));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn shift_enter_inserts_a_composer_newline_without_submitting() {
        let mut app = test_app(value!({}), 0);
        app.composer.set_text("first");
        let mut shift_enter = Key::new(KeyCode::Enter);
        shift_enter.shift = true;

        let action = app.handle(&Event::Key(shift_enter));

        assert_eq!(action, AppAction::Continue);
        assert_eq!(app.composer.text(), "first\n");
    }

    #[test]
    fn raw_terminal_newline_encoding_inserts_a_composer_newline() {
        let mut app = test_app(value!({}), 0);
        app.composer.set_text("first");

        let action = app.handle(&Event::Key(Key::new(KeyCode::Char('\n'))));

        assert_eq!(action, AppAction::Continue);
        assert_eq!(app.composer.text(), "first\n");
    }

    #[test]
    fn shift_tab_moves_backward_through_panels() {
        let mut app = test_app(value!({}), 0);
        let shift_tab = Key::new(KeyCode::BackTab);

        let _ = app.handle(&Event::Key(shift_tab));
        assert_eq!(app.focus, Focus::Preview);
        let _ = app.handle(&Event::Key(shift_tab));
        assert_eq!(app.focus, Focus::Memory);
        let _ = app.handle(&Event::Key(shift_tab));
        assert_eq!(app.focus, Focus::Composer);
    }

    #[test]
    fn selecting_another_memory_item_starts_its_preview_at_the_top() {
        let mut app = test_app(value!({"thread": {}, "memory": {}}), 0);
        app.focus = Focus::Memory;
        app.preview_scroll.set_offset(10);
        let memory = app
            .rows
            .iter()
            .position(|row| row.path == "/memory")
            .unwrap();
        app.tree.select(Some(memory));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Down)));

        assert_eq!(app.selected().path, "/thread");
        assert_eq!(app.preview_scroll.offset(), 0);
    }

    #[test]
    fn memory_nodes_collapse_expand_and_stay_collapsed_after_refresh() {
        let mut app = test_app(value!({"thread": {"messages": ["hello"]}, "memory": {}}), 0);
        app.focus = Focus::Memory;
        let thread = app
            .rows
            .iter()
            .position(|row| row.path == "/thread")
            .unwrap();
        app.tree.select(Some(thread));

        assert!(app.selected().label.contains("▸ thread"));
        assert!(!app.rows.iter().any(|row| row.path == "/thread/messages"));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert!(app.selected().label.contains("▾ thread"));
        assert!(app.rows.iter().any(|row| row.path == "/thread/messages"));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert!(app.selected().label.contains("▸ thread"));
        assert!(!app.rows.iter().any(|row| row.path == "/thread/messages"));
        app.refresh_process(
            value!({"thread": {"messages": ["hello", "again"]}, "memory": {}}),
            1,
        );
        assert!(!app.rows.iter().any(|row| row.path == "/thread/messages"));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Right)));

        assert_eq!(app.selected().path, "/thread");
        assert!(app.selected().label.contains("▾ thread"));
        assert!(app.rows.iter().any(|row| row.path == "/thread/messages"));
    }

    #[test]
    fn left_on_a_closed_node_moves_to_its_parent() {
        let mut app = test_app(value!({"memory": {"profile": {"name": "Ada"}}}), 0);
        app.focus = Focus::Memory;
        expand_paths(&mut app, &["/memory", "/memory/profile"]);
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
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        app.focus = Focus::Memory;
        app.memory_viewport_height = 4;
        expand_paths(&mut app, &["/memory"]);

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageDown)));
        assert_eq!(app.tree.selected(), Some(3));

        let _ = app.handle(&Event::Mouse(Mouse::at(MouseKind::ScrollDown, 0, 0)));
        assert_eq!(app.tree.selected(), Some(6));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageUp)));
        assert_eq!(app.tree.selected(), Some(3));
    }

    #[test]
    fn mouse_wheel_over_memory_routes_to_memory_without_keyboard_focus() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        app.memory_viewport_height = 4;
        expand_paths(&mut app, &["/memory"]);
        let probes = layout(&mut app, 90, 24);

        let _ = app.handle(&panel_mouse(MouseKind::ScrollDown, probes.memory.rect()));

        assert_eq!(app.focus, Focus::Memory);
        assert_eq!(app.tree.selected(), Some(3));
    }

    #[test]
    fn clicking_a_panel_activates_it() {
        let mut app = test_app(value!({"memory": {}}), 0);
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
    fn clicking_a_memory_row_selects_it() {
        let mut app = test_app(value!({"memory": {"alpha": 1, "beta": 2}}), 0);
        expand_paths(&mut app, &["/memory"]);
        let probes = layout(&mut app, 90, 24);
        let bounds = probes.memory.rect();

        let _ = app.handle(&Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            bounds.x + 2,
            bounds.y + 3,
        )));

        assert_eq!(app.focus, Focus::Memory);
        assert_eq!(app.selected().path, "/memory/alpha");
    }

    #[test]
    fn clicking_a_scrolled_memory_row_uses_the_visible_offset() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/8")
            .unwrap();
        app.tree.select(Some(selected));
        let probes = layout(&mut app, 90, 8);
        let first_visible = app.memory_window().start();
        let expected_path = app.rows[first_visible].path.clone();
        let bounds = probes.memory.rect();

        let _ = app.handle(&Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            bounds.x + 2,
            bounds.y + 1,
        )));

        assert_eq!(app.selected().path, expected_path);
    }

    #[test]
    fn clicking_the_bottom_row_keeps_the_memory_window_stable() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/5")
            .unwrap();
        app.tree.select(Some(selected));
        let probes = layout(&mut app, 90, 8);
        let first_visible = app.memory_window().start();
        let body = panel_body(probes.memory.rect());

        let _ = app.handle(&Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            body.x + 1,
            body.bottom() - 1,
        )));

        assert_eq!(app.memory_window().start(), first_visible);
    }

    #[test]
    fn memory_tree_viewport_follows_an_offscreen_selection() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        app.memory_viewport_height = 4;
        let selected = app
            .rows
            .iter()
            .position(|row| row.path == "/memory/8")
            .unwrap();
        app.tree.select(Some(selected));
        app.keep_tree_selection_visible();
        let theme = lampa_theme();

        let frame = grid(&render(tree_view(&app, &theme).as_ref(), 30, 6, &theme));

        assert!(frame.contains("[8]"));
        assert!(!frame.contains("[0]"));
    }

    #[test]
    fn headless_frame_contains_tree_chat_and_status() {
        let mut app = test_app_with(
            ValueView::new(value!({
                "thread": {"messages": []},
                "memory": {"release": {"color": "blue"}}
            })),
            3,
            "sim",
        );
        app.push_user("Remember the release color.".into());
        app.push_assistant(Message::assistant("Stored in memory."));
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(&mut app, Rect::new(0, 0, 90, 24), &theme, &probes);

        let frame = grid(&render(root.as_ref(), 90, 24, &theme));

        assert!(!frame.contains("Inbox / Outbox"));
        assert!(frame.contains(" Memory "));
        assert!(frame.contains("thread"));
        assert!(frame.contains("memory"));
        assert!(frame.contains("Stored in memory."));
        assert!(frame.contains("v3 · sim · ready"));
    }

    #[test]
    fn reasoning_failure_is_visible_as_an_event_and_status() {
        let mut app = test_app_with(ValueView::new(value!({})), 3, "sim");
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
