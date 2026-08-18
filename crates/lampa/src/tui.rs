use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event;
use ratatui::{Terminal, TerminalOptions, Viewport, buffer::Buffer};
use svit::{
    Change, ContentPart, Events, Inbox, Message, MessageRole, Outbox, Svit, SvitEvent, Value,
};
use tuika::components::{TreeList, TreeRow, TreeState};
use tuika::mouse::{SelectionState, paint_selection, selected_text};
use tuika::prelude::*;
use tuika::probe::RectProbe;
use tuika::term::{
    clipboard,
    hyperlink::{self, HyperlinkBackend},
};
use tuika_codeformatters::TreeSitterHighlighter;

type MemoryTreeRow = TreeRow<'static, String>;

const MEMORY_WIDTH: u16 = 30;
const FRAME_TIME: Duration = Duration::from_millis(50);
/// Bounds input draining so continuous pointer motion cannot starve rendering.
const MAX_READY_INPUT_EVENTS: usize = 64;
const MAX_PREVIEW_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_ITEMS: usize = 200;
const MAX_PREVIEW_DEPTH: usize = 2;
const MAX_INLINE_VALUE_BYTES: usize = 160;
const MAX_TREE_ITEM_PREVIEW_BYTES: usize = 48;
const RECENT_MESSAGES_LIMIT: usize = 500;
/// Bounded, read-only EventLog window projected beneath `/thread` for Lampa.
const RECENT_EVENTS_LIMIT: usize = 200;
/// Children shown under one directory row.
const MAX_TREE_CHILDREN: usize = 200;
const UKRAINE_BANNER: &str = "Світ, це Світ. Зроблено в Україні";
const YOLOP_BLUE: &str = "\x1b[38;2;45;91;158m";
const YOLOP_GOLD: &str = "\x1b[38;2;126;94;19m";
const ANSI_RESET: &str = "\x1b[0m";

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
    transcript: RectProbe,
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

/// Resolves a deliberate Ctrl/Cmd-click against the last rendered transcript.
/// The target is either an explicit Markdown href or a visible HTTP(S) URL.
fn transcript_link_click(mouse: &Mouse, buffer: &Buffer, area: Rect) -> Option<String> {
    let mut activation = *mouse;
    activation.ctrl |= activation.super_key;
    hyperlink::ctrl_click_url(&activation, buffer, area)
}

/// Collects terminal input already available after the first event.
///
/// A trackpad can queue many wheel events between frames. Draining that queue
/// lets a reversal take effect on the next render rather than replaying stale
/// movement one event per frame.
fn ready_terminal_events(
    first: event::Event,
    mut is_ready: impl FnMut() -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<event::Event>,
) -> io::Result<Vec<event::Event>> {
    let mut events = vec![first];
    while events.len() < MAX_READY_INPUT_EVENTS && is_ready()? {
        events.push(read()?);
    }
    Ok(events)
}

/// Opens a user-selected web URL without invoking a shell.
#[cfg(target_os = "macos")]
fn open_link(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {url}: {error}"))
}

#[cfg(target_os = "linux")]
fn open_link(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {url}: {error}"))
}

#[cfg(target_os = "windows")]
fn open_link(url: &str) -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {url}: {error}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn open_link(_url: &str) -> Result<(), String> {
    Err("opening links is not supported on this platform".into())
}

fn panel_body(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
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

/// Lampa's bounded presentation cache for canonical thread history.
///
/// This is deliberately outside Svit's process tree: history remains paged
/// EventLog infrastructure and therefore cannot grow snapshots or resume work.
#[derive(Clone)]
struct ThreadHistory {
    events: Value,
    messages: Value,
}

impl Default for ThreadHistory {
    fn default() -> Self {
        Self {
            events: Value::Array(Vec::new()),
            messages: Value::Array(Vec::new()),
        }
    }
}

impl ThreadHistory {
    fn set_events(&mut self, events: Vec<Value>) {
        self.events = Value::Array(events);
    }

    fn push_event(&mut self, event: Value) {
        let Value::Array(events) = &mut self.events else {
            self.events = Value::Array(vec![event]);
            return;
        };
        if events.len() == RECENT_EVENTS_LIMIT {
            events.remove(0);
        }
        events.push(event);
    }

    fn push_message(&mut self, message: Value) {
        let Value::Array(messages) = &mut self.messages else {
            self.messages = Value::Array(vec![message]);
            return;
        };
        if messages.len() == RECENT_MESSAGES_LIMIT {
            messages.remove(0);
        }
        messages.push(message);
    }

    fn at(&self, path: &str) -> Option<&Value> {
        let (value, suffix) = if path == "/thread/events" {
            (&self.events, "")
        } else if path == "/thread/messages" {
            (&self.messages, "")
        } else if let Some(suffix) = path.strip_prefix("/thread/events/") {
            (&self.events, suffix)
        } else if let Some(suffix) = path.strip_prefix("/thread/messages/") {
            (&self.messages, suffix)
        } else {
            return None;
        };
        let mut current = value;
        for segment in suffix.split('/').filter(|segment| !segment.is_empty()) {
            current = match current {
                Value::Map(values) => values.get(segment)?,
                Value::Array(values) => values.get(segment.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }
}

/// Read-only Lampa overlay that makes bounded EventLog history browseable
/// beneath `/thread` without changing the Svit memory tree.
struct ThreadHistoryView<'a, V: MemoryView + ?Sized> {
    process: &'a V,
    history: &'a ThreadHistory,
}

impl<'a, V: MemoryView + ?Sized> ThreadHistoryView<'a, V> {
    fn new(process: &'a V, history: &'a ThreadHistory) -> Self {
        Self { process, history }
    }
}

impl<V: MemoryView + ?Sized> MemoryView for ThreadHistoryView<'_, V> {
    fn discover(&self, path: &str) -> Result<Vec<String>, String> {
        if path == "/thread" {
            let mut names = self.process.discover(path)?;
            for name in ["events", "messages"] {
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.into());
                }
            }
            names.sort();
            return Ok(names);
        }
        match self.history.at(path) {
            Some(Value::Map(values)) => Ok(values.keys().cloned().collect()),
            Some(Value::Array(values)) => {
                Ok((0..values.len()).map(|index| index.to_string()).collect())
            }
            Some(_) => Err(format!("invalid state path: {path}")),
            None => self.process.discover(path),
        }
    }

    fn stat(&self, path: &str) -> Result<Option<Value>, String> {
        match self.history.at(path) {
            Some(value) => Ok(Some(value_facts(path, value))),
            None => self.process.stat(path),
        }
    }

    fn read(&self, path: &str) -> Result<Option<Value>, String> {
        if let Some(value) = self.history.at(path) {
            return Ok(Some(value.clone()));
        }
        self.process.read(path)
    }

    fn version(&self) -> u64 {
        self.process.version()
    }
}

fn value_facts(path: &str, value: &Value) -> Value {
    let (kind, content, entries) = match value {
        Value::Map(values) => ("directory", "object", Some(values.len())),
        Value::Array(values) => ("directory", "array", Some(values.len())),
        Value::String(_) => ("leaf", "text/plain", None),
        Value::Script(_) => ("leaf", "svit-script", None),
        _ => ("leaf", "scalar", None),
    };
    let mut facts = BTreeMap::from([("content".into(), Value::from(content))]);
    if let Some(entries) = entries {
        facts.insert("entries".into(), Value::Integer(entries as i64));
    }
    Value::Map(BTreeMap::from([
        ("kind".into(), Value::from(kind)),
        ("facts".into(), Value::Map(facts)),
        ("locality".into(), Value::from("cache")),
        ("path".into(), Value::from(path)),
    ]))
}

fn serialized_value(value: serde_json::Value) -> Value {
    Value::from_json(value).expect("Svit history values are valid JSON")
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
    /// Committed content hash, absent for a mount node or an unreachable one.
    hash: Option<String>,
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
            hash: text(fields, "hash"),
        }
    }

    fn missing() -> Self {
        Self {
            directory: false,
            locality: "cache".into(),
            content: String::new(),
            hash: None,
        }
    }

    fn unreachable() -> Self {
        Self {
            directory: false,
            locality: "remote".into(),
            content: String::new(),
            hash: None,
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
    /// Entries a commit could have made stale, kept until read again.
    stale_nodes: BTreeSet<String>,
    stale_children: BTreeSet<String>,
    stale_values: BTreeSet<String>,
    /// Bumped whenever a resolution replaces an entry.
    revision: u64,
}

impl MemoryTree {
    fn clear(&mut self) {
        self.nodes.clear();
        self.children.clear();
        self.values.clear();
        self.truncated.clear();
        self.stale_nodes.clear();
        self.stale_children.clear();
        self.stale_values.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Marks every entry the change could have made stale.
    ///
    /// A commit never blanks the console. Any change reaches the root, because
    /// a write below a node changes that node's value and can change its child
    /// listing, so discarding the touched entries would leave nothing to paint
    /// until the next resolution. The last resolved facts stay on screen and
    /// each one is replaced when it is read again.
    fn invalidate(&mut self, change: &Change) {
        // A published content hash equal to the cached one settles the
        // question: that subtree is the one already on screen, whatever the
        // path overlap says.
        let current = |path: &String| match (change.hash(path), self.nodes.get(path)) {
            (Some(Some(published)), Some(node)) => node.hash.as_deref() == Some(published),
            _ => false,
        };
        let stale = |path: &String| change.touches(path) && !current(path);
        let nodes = self.nodes.keys().filter(|path| stale(path)).cloned();
        let children = self.children.keys().filter(|path| stale(path)).cloned();
        let values = self.values.keys().filter(|path| stale(path)).cloned();
        let (nodes, children, values) = (
            nodes.collect::<Vec<_>>(),
            children.collect::<Vec<_>>(),
            values.collect::<Vec<_>>(),
        );
        self.stale_nodes.extend(nodes);
        self.stale_children.extend(children);
        self.stale_values.extend(values);
    }

    /// Marks one host overlay subtree stale without claiming a process commit.
    ///
    /// Lampa owns the bounded thread projection under `/thread`, so appending
    /// to it changes nothing the process committed and must not invalidate
    /// the committed tree around it.
    fn invalidate_overlay(&mut self, path: &str) {
        let below = |candidate: &String| {
            candidate == path
                || candidate
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let nodes = self.nodes.keys().filter(|p| below(p)).cloned();
        let children = self.children.keys().filter(|p| below(p)).cloned();
        let values = self.values.keys().filter(|p| below(p)).cloned();
        let (nodes, children, values) = (
            nodes.collect::<Vec<_>>(),
            children.collect::<Vec<_>>(),
            values.collect::<Vec<_>>(),
        );
        self.stale_nodes.extend(nodes);
        self.stale_children.extend(children);
        self.stale_values.extend(values);
    }

    fn revision(&self) -> u64 {
        self.revision
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
        if self.nodes.contains_key(path) && !self.stale_nodes.contains(path) {
            return;
        }
        let node = match view.stat(path) {
            Ok(Some(facts)) => Node::from_facts(&facts),
            Ok(None) => Node::missing(),
            Err(error) => {
                // An unreachable node still occupies a row; its failure becomes
                // the value the preview shows.
                self.values.insert(path.to_owned(), Value::String(error));
                self.stale_values.remove(path);
                Node::unreachable()
            }
        };
        self.nodes.insert(path.to_owned(), node);
        self.stale_nodes.remove(path);
        self.revision = self.revision.wrapping_add(1);
    }

    fn resolve_children(&mut self, view: &dyn MemoryView, path: &str) {
        let listed = self.children.contains_key(path) && !self.stale_children.contains(path);
        if listed || !self.is_directory(path) {
            return;
        }
        let mut names = match view.discover(path) {
            Ok(names) => names,
            Err(error) => {
                self.children.insert(path.to_owned(), Vec::new());
                self.values.insert(path.to_owned(), Value::String(error));
                self.stale_children.remove(path);
                self.stale_values.remove(path);
                self.revision = self.revision.wrapping_add(1);
                return;
            }
        };
        // A directory has no committed size, so one listing is bounded here
        // rather than trusting the source.
        self.truncated.remove(path);
        if names.len() > MAX_TREE_CHILDREN {
            names.truncate(MAX_TREE_CHILDREN);
            self.truncated.insert(path.to_owned());
        }
        for name in &names {
            self.resolve_node(view, &child_path(path, name));
        }
        // A refreshed listing drops the rows the process no longer reports.
        if let Some(previous) = self.children.insert(path.to_owned(), names) {
            let retained = self.children[path].clone();
            for name in previous {
                if !retained.contains(&name) {
                    self.forget(&child_path(path, &name));
                }
            }
        }
        self.stale_children.remove(path);
        self.revision = self.revision.wrapping_add(1);
    }

    fn resolve_value(&mut self, view: &dyn MemoryView, path: &str) {
        if self.values.contains_key(path) && !self.stale_values.contains(path) {
            return;
        }
        let value = match view.read(path) {
            Ok(Some(value)) => value,
            Ok(None) => Value::Null,
            Err(error) => Value::String(error),
        };
        self.values.insert(path.to_owned(), value);
        self.stale_values.remove(path);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Drops one path and its descendants once the process stops reporting it.
    fn forget(&mut self, path: &str) {
        let below = |candidate: &String| {
            candidate == path
                || candidate
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        self.nodes.retain(|candidate, _| !below(candidate));
        self.children.retain(|candidate, _| !below(candidate));
        self.values.retain(|candidate, _| !below(candidate));
        self.truncated.retain(|candidate| !below(candidate));
        self.stale_nodes.retain(|candidate| !below(candidate));
        self.stale_children.retain(|candidate| !below(candidate));
        self.stale_values.retain(|candidate| !below(candidate));
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
    tree: TreeState<String>,
    nodes: MemoryTree,
    memory_viewport_height: usize,
    memory_window: VirtualWindow,
    panel_bounds: PanelBounds,
    selection_area: Rect,
    selection: SelectionState,
    pending_copy: bool,
    composer: TextInputState,
    transcript_scroll: ScrollState,
    /// Wrapped transcript rows from the frame that established the current viewport.
    transcript_content_height: usize,
    transcript_viewport_height: usize,
    preview_scroll: ScrollState,
    preview_content_height: usize,
    preview_cache: Option<PreviewCache>,
    rows: Vec<MemoryTreeRow>,
    thread_history: Arc<ThreadHistory>,
    /// A row path to reselect once the tree resolves it again.
    pending_selection: Option<String>,
    timeline: Vec<TimelineEntry>,
    seen_message_ids: BTreeSet<String>,
    /// Local input IDs awaiting Everruns' canonical replacement message.
    pending_user_ids: VecDeque<String>,
    working: bool,
    failure: Option<String>,
    version: u64,
    model: String,
}

impl App {
    fn new(version: u64, model: String) -> Self {
        let mut preview_scroll = ScrollState::new();
        preview_scroll.jump_to_top();
        let nodes = MemoryTree::default();
        let mut tree = TreeState::with_selected("/".into());
        tree.expand("/".into());
        let rows = tree_rows(&nodes);
        Self {
            focus: Focus::Composer,
            tree,
            nodes,
            // The first paint has no captured panel bounds. TreeList clamps
            // this window to the actual allocation, then later frames retain
            // the viewport resolved from those bounds.
            memory_viewport_height: usize::MAX,
            memory_window: VirtualWindow::new(1, 1, 0),
            panel_bounds: PanelBounds::default(),
            selection_area: Rect::default(),
            selection: SelectionState::new(),
            pending_copy: false,
            composer: TextInputState::new(),
            transcript_scroll: ScrollState::new(),
            transcript_content_height: 1,
            transcript_viewport_height: 1,
            preview_scroll,
            preview_content_height: 1,
            preview_cache: None,
            rows,
            thread_history: Arc::new(ThreadHistory::default()),
            pending_selection: None,
            timeline: Vec::new(),
            seen_message_ids: BTreeSet::new(),
            pending_user_ids: VecDeque::new(),
            working: false,
            failure: None,
            version,
            model,
        }
    }

    fn selected(&self) -> &MemoryTreeRow {
        self.tree
            .selected()
            .and_then(|path| self.rows.iter().find(|row| &row.id == path))
            .unwrap_or_else(|| self.rows.first().expect("the memory tree has a root"))
    }

    fn selected_value(&self) -> &Value {
        self.nodes
            .value(&self.selected().id)
            .unwrap_or(&PENDING_VALUE)
    }

    /// Resolves what the current tree and selection need, and nothing else.
    ///
    /// Runs once per frame. Every node goes through the same three process
    /// operations, so a mounted folder and committed memory are browsed the
    /// same way.
    fn resolve(&mut self, view: &dyn MemoryView) {
        let before = self.nodes.revision();
        self.nodes.resolve_node(view, "/");
        // Expanded paths iterate parent before child, so a directory's kind is
        // known by the time its own listing is requested.
        for path in self.tree.expanded().cloned().collect::<Vec<_>>() {
            self.nodes.resolve_children(view, &path);
        }
        self.rows = tree_rows(&self.nodes);
        // A commit discards every resolved node, so the row the operator was
        // reading is restored once its ancestors are listed again.
        if let Some(path) = self.pending_selection.take() {
            self.tree.select(Some(path));
        }

        // A row needs a value only to summarize an array item; the selected
        // row needs one for its preview. Content is read eagerly only where
        // the process reports it is already resident.
        let summarized = self
            .rows
            .iter()
            .filter(|row| {
                self.nodes.node(&row.id).is_some_and(|node| node.resident())
                    && self
                        .nodes
                        .node(&parent_path(&row.id))
                        .is_some_and(Node::array)
            })
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        for path in summarized {
            self.nodes.resolve_value(view, &path);
        }
        let selected = self.selected().id.clone();
        self.nodes.resolve_value(view, &selected);

        if before != self.nodes.revision() {
            self.preview_cache = None;
            self.reconcile_tree();
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
            self.pending_selection = Some(self.selected().id.clone());
        }
        if change.paths().is_empty() {
            self.nodes.clear();
        } else {
            self.nodes.invalidate(change);
        }
        self.rows = tree_rows(&self.nodes);
        self.preview_cache = None;
        self.version = change.version();
    }

    /// Drops every resolved node so the next frame reads the tree again.
    fn reload(&mut self) {
        self.pending_selection = Some(self.selected().id.clone());
        self.nodes.clear();
        self.rows = tree_rows(&self.nodes);
        self.preview_cache = None;
    }

    fn reconcile_tree(&mut self) {
        self.memory_window = self.tree.resolve(&self.rows, self.memory_viewport_height);
    }

    fn push_user(&mut self, message: Message) {
        let id = message.id.to_string();
        if self.push_message(message) {
            self.pending_user_ids.push_back(id);
        }
        self.working = true;
    }

    fn push_message(&mut self, message: Message) -> bool {
        if self.reconcile_pending_user(&message) {
            return false;
        }
        if self.seen_message_ids.insert(message.id.to_string()) {
            Arc::make_mut(&mut self.thread_history).push_message(serialized_value(
                serde_json::to_value(&message).expect("Svit messages serialize"),
            ));
            self.refresh_thread_history();
            self.clear_selection();
            self.timeline
                .push(TimelineEntry::Message(Box::new(message)));
            return true;
        }
        false
    }

    fn set_thread_events(&mut self, events: Vec<Value>) {
        Arc::make_mut(&mut self.thread_history).set_events(events);
        self.refresh_thread_history();
    }

    fn push_thread_event(&mut self, event: Value) {
        Arc::make_mut(&mut self.thread_history).push_event(event);
        self.refresh_thread_history();
    }

    fn refresh_thread_history(&mut self) {
        // The thread projection is a host overlay, not committed state: the
        // process neither changed nor advanced its version, so only the
        // overlay rows are read again.
        for path in ["/thread/events", "/thread/messages"] {
            self.nodes.invalidate_overlay(path);
        }
        self.rows = tree_rows(&self.nodes);
    }

    fn reconcile_pending_user(&mut self, canonical: &Message) -> bool {
        if canonical.role != MessageRole::User {
            return false;
        }
        let Some(pending_id) = self.pending_user_ids.front() else {
            return false;
        };
        let Some(TimelineEntry::Message(pending)) = self.timeline.iter_mut().find(|entry| {
            matches!(
                entry,
                TimelineEntry::Message(message) if message.id.to_string() == *pending_id
            )
        }) else {
            return false;
        };
        if pending.content != canonical.content
            || pending.controls != canonical.controls
            || pending.metadata != canonical.metadata
        {
            return false;
        }

        let pending_id = self
            .pending_user_ids
            .pop_front()
            .expect("the pending user message was just resolved");
        self.seen_message_ids.remove(&pending_id);
        self.seen_message_ids.insert(canonical.id.to_string());
        **pending = canonical.clone();
        true
    }

    fn finish_turn(&mut self) {
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
        self.selection_area = probes.transcript.rect();
        self.transcript_viewport_height = usize::from(self.selection_area.height).max(1);
        self.transcript_scroll.clamp(
            self.transcript_content_height,
            self.transcript_viewport_height,
        );
    }

    /// Routes mouse capture through Tuika's text selection before panel clicks.
    ///
    /// A drag must begin in the transcript. Subsequent drag and release cells
    /// are clamped to its visible rect, while a press elsewhere clears the
    /// highlight and remains available to the memory tree or preview.
    fn route_selection_mouse(&mut self, mouse: &Mouse) -> bool {
        if !mouse.plain() {
            return false;
        }
        match mouse.kind {
            MouseKind::Down(MouseButton::Left) => {
                if !contains(self.selection_area, mouse) {
                    self.clear_selection();
                    return false;
                }
                self.focus = Focus::Composer;
                self.pending_copy = false;
                let _ = self.selection.handle(mouse);
                true
            }
            MouseKind::Drag(MouseButton::Left) | MouseKind::Up(MouseButton::Left) => {
                if self.selection_area.width == 0 || self.selection_area.height == 0 {
                    return false;
                }
                let mut bounded = *mouse;
                bounded.column = bounded.column.clamp(
                    self.selection_area.x,
                    self.selection_area.right().saturating_sub(1),
                );
                bounded.row = bounded.row.clamp(
                    self.selection_area.y,
                    self.selection_area.bottom().saturating_sub(1),
                );
                let changed = self.selection.handle(&bounded);
                if changed
                    && matches!(mouse.kind, MouseKind::Up(MouseButton::Left))
                    && self.selection.range().is_some()
                {
                    self.pending_copy = true;
                }
                changed
            }
            _ => false,
        }
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.pending_copy = false;
    }

    /// Resolves double-click selection, extracts deferred clipboard text, and
    /// paints the active range over the finished transcript frame.
    fn finish_selection_frame(&mut self, buffer: &mut Buffer, theme: &Theme) -> Option<String> {
        if self.selection.resolve(buffer, self.selection_area) {
            self.pending_copy = true;
        }
        let Some(range) = self.selection.range() else {
            self.pending_copy = false;
            return None;
        };
        let copied = self
            .pending_copy
            .then(|| selected_text(buffer, self.selection_area, range));
        self.pending_copy = false;
        paint_selection(
            buffer,
            self.selection_area,
            range,
            Style::default()
                .fg(theme.selection_fg)
                .bg(theme.selection_bg),
        );
        copied
    }

    fn route_mouse(&mut self, event: &Event) -> bool {
        let Event::Mouse(mouse) = event else {
            return false;
        };
        if self.route_selection_mouse(mouse) {
            return true;
        }
        let Some(target) = self.panel_bounds.focus_at(mouse) else {
            return false;
        };
        match mouse.kind {
            MouseKind::Down(MouseButton::Left) if mouse.plain() => {
                self.focus = target;
                if target == Focus::Memory {
                    let selected = self.tree.selected().cloned();
                    let _ = self.tree.handle_mouse(
                        event,
                        &self.rows,
                        panel_body(self.panel_bounds.memory),
                        self.memory_window,
                    );
                    if self.tree.selected() != selected.as_ref() {
                        self.preview_scroll.jump_to_top();
                    }
                    self.reconcile_tree();
                }
                true
            }
            MouseKind::ScrollUp | MouseKind::ScrollDown if mouse.plain() => {
                self.clear_selection();
                self.focus = target;
                let down = mouse.kind == MouseKind::ScrollDown;
                match target {
                    Focus::Memory => {
                        let selected = self.tree.selected().cloned();
                        let key = if down { KeyCode::Down } else { KeyCode::Up };
                        for _ in 0..3 {
                            let _ = self.tree.handle(
                                &Event::Key(Key::new(key)),
                                &self.rows,
                                self.memory_viewport_height,
                            );
                        }
                        if self.tree.selected() != selected.as_ref() {
                            self.preview_scroll.jump_to_top();
                        }
                        self.reconcile_tree();
                    }
                    Focus::Composer => {
                        let _ = self.transcript_scroll.handle(
                            event,
                            self.transcript_content_height,
                            self.transcript_viewport_height,
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
            if self.selection.is_active() {
                self.pending_copy = true;
                return AppAction::Continue;
            }
            return AppAction::Quit;
        }

        if matches!(event, Event::Key(_) | Event::Resize { .. }) {
            self.clear_selection();
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
                let selected = self.tree.selected().cloned();
                match event {
                    // Nothing reports an external change to a mounted source,
                    // so re-reading the tree stays a deliberate action.
                    Event::Key(Key {
                        code: KeyCode::Char('r'),
                        ..
                    }) => self.reload(),
                    Event::Key(Key {
                        code: KeyCode::PageUp,
                        ..
                    }) => {
                        for _ in 0..self.memory_viewport_height.saturating_sub(1).max(1) {
                            let _ = self.tree.handle(
                                &Event::Key(Key::new(KeyCode::Up)),
                                &self.rows,
                                self.memory_viewport_height,
                            );
                        }
                    }
                    Event::Key(Key {
                        code: KeyCode::PageDown,
                        ..
                    }) => {
                        for _ in 0..self.memory_viewport_height.saturating_sub(1).max(1) {
                            let _ = self.tree.handle(
                                &Event::Key(Key::new(KeyCode::Down)),
                                &self.rows,
                                self.memory_viewport_height,
                            );
                        }
                    }
                    Event::Mouse(Mouse {
                        kind: MouseKind::ScrollUp,
                        ..
                    }) => {
                        for _ in 0..3 {
                            let _ = self.tree.handle(
                                &Event::Key(Key::new(KeyCode::Up)),
                                &self.rows,
                                self.memory_viewport_height,
                            );
                        }
                    }
                    Event::Mouse(Mouse {
                        kind: MouseKind::ScrollDown,
                        ..
                    }) => {
                        for _ in 0..3 {
                            let _ = self.tree.handle(
                                &Event::Key(Key::new(KeyCode::Down)),
                                &self.rows,
                                self.memory_viewport_height,
                            );
                        }
                    }
                    _ => {
                        let _ = self
                            .tree
                            .handle(event, &self.rows, self.memory_viewport_height);
                    }
                }
                if self.tree.selected() != selected.as_ref() {
                    self.preview_scroll.jump_to_top();
                }
                self.reconcile_tree();
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
                    let _ = self.transcript_scroll.handle(
                        event,
                        self.transcript_content_height,
                        self.transcript_viewport_height,
                    );
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
    let recent_events = svit
        .recent_events(RECENT_EVENTS_LIMIT)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|event| serialized_value(serde_json::to_value(event).expect("Svit events serialize")))
        .collect();
    let mut app = App::new(MemoryView::version(&svit), model);
    app.set_thread_events(recent_events);
    for message in svit
        .recent_messages(RECENT_MESSAGES_LIMIT)
        .await
        .map_err(|error| error.to_string())?
    {
        app.push_message(message);
    }
    let inbox = svit.inbox();
    let mut events = svit.events();
    let mut outbox = svit.outbox();
    svit.start().map_err(|error| error.to_string())?;

    let ui_result = run_terminal(&mut app, &svit, &inbox, &mut events, &mut outbox).await;
    let block_result = svit.block().await.map_err(|error| error.to_string());
    if ui_result.is_ok() && block_result.is_ok() {
        print_centered_ukraine_banner();
    }
    block_result?;
    ui_result
}

/// Prints Lampa's Ukrainian signature after the terminal session is restored.
fn print_centered_ukraine_banner() {
    let width = crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(0);
    println!("{}", centered_ukraine_banner(width));
}

fn centered_ukraine_banner(width: usize) -> String {
    let pad = width.saturating_sub(UKRAINE_BANNER.chars().count()) / 2;
    format!(
        "{}{}Світ, це Світ. Зроблено в {}Україні{}",
        " ".repeat(pad),
        YOLOP_BLUE,
        YOLOP_GOLD,
        ANSI_RESET,
    )
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
                    SvitEvent::CanonicalEvent(event) => app.push_thread_event(serialized_value(
                        serde_json::to_value(&*event).expect("Svit events serialize"),
                    )),
                    SvitEvent::Message(message) => {
                        app.push_message(*message);
                    }
                    SvitEvent::Failed(error) => app.push_error(error),
                }
            }
            let mut turn_completed = false;
            while outbox.try_recv().is_ok() {
                turn_completed = true;
            }
            if turn_completed {
                app.finish_turn();
            }
            // The tree resolves here, once per frame, so the console fetches
            // only the nodes the visible tree and current selection need.
            let history = Arc::clone(&app.thread_history);
            let view = ThreadHistoryView::new(svit, &history);
            app.resolve(&view);

            let mut copied = None;
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let root = build_view(app, area, &theme, &probes);
                    paint(frame.buffer_mut(), area, &theme, root.as_ref(), &[]);
                    app.capture_panel_bounds(&probes);
                    copied = app.finish_selection_frame(frame.buffer_mut(), &theme);
                    if let Some(position) = app.cursor(&probes) {
                        frame.set_cursor_position(position);
                    }
                })
                .map_err(|error| error.to_string())?;
            if let Some(text) = copied {
                clipboard::write(&mut io::stdout(), &text).map_err(|error| error.to_string())?;
            }

            if event::poll(FRAME_TIME).map_err(|error| error.to_string())? {
                let mut quit = false;
                let first = event::read().map_err(|error| error.to_string())?;
                let ready =
                    ready_terminal_events(first, || event::poll(Duration::ZERO), event::read)
                        .map_err(|error| error.to_string())?;
                for raw_event in ready {
                    if let Some(event) = translate_event(raw_event) {
                        if let Event::Mouse(mouse) = &event
                            && let Some(url) = transcript_link_click(
                                mouse,
                                terminal.current_buffer_mut(),
                                app.selection_area,
                            )
                        {
                            if let Err(error) = open_link(&url) {
                                app.timeline.push(TimelineEntry::Event {
                                    label: "ERROR",
                                    text: error,
                                });
                            }
                            continue;
                        }
                        match app.handle(&event) {
                            AppAction::Continue => {}
                            AppAction::Submit(text) => {
                                let message = Message::user(text);
                                inbox
                                    .send(message.clone())
                                    .await
                                    .map_err(|error| error.to_string())?;
                                app.push_user(message);
                            }
                            AppAction::Quit => {
                                quit = true;
                                break;
                            }
                        }
                    }
                }
                if quit {
                    break;
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = terminal.clear();
    result
}

fn tree_rows(nodes: &MemoryTree) -> Vec<MemoryTreeRow> {
    let mut rows = vec![TreeRow::root("/".into(), "/", nodes.is_directory("/"))];
    flatten_tree("/", 0, nodes, &mut rows);
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

fn flatten_tree(path: &str, depth: usize, nodes: &MemoryTree, rows: &mut Vec<MemoryTreeRow>) {
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

    for (label, child, expandable) in children {
        rows.push(TreeRow::new(
            child.clone(),
            Some(path.to_owned()),
            depth + 1,
            label,
            expandable,
        ));
        flatten_tree(&child, depth + 1, nodes, rows);
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
    app.reconcile_tree();
    let preview_content_width = preview_width.saturating_sub(4).max(1);
    let selected_path = app.selected().id.clone();
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
                &probes.transcript,
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

struct MemoryTreeView {
    rows: Vec<MemoryTreeRow>,
    state: TreeState<String>,
    window: VirtualWindow,
}

impl View for MemoryTreeView {
    fn measure(&self, available: Size, ctx: &RenderCtx) -> Size {
        TreeList::new(&self.rows, &self.state)
            .visible_window(self.window)
            .scrollbar(true)
            .measure(available, ctx)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        TreeList::new(&self.rows, &self.state)
            .visible_window(self.window)
            .scrollbar(true)
            .render(area, surface, ctx);
    }
}

fn tree_view(app: &App, theme: &Theme) -> Element {
    let border = if app.focus == Focus::Memory {
        theme.border_focused
    } else {
        theme.border
    };
    element(
        Boxed::new(element(MemoryTreeView {
            rows: app.rows.clone(),
            state: app.tree.clone(),
            window: app.memory_window,
        }))
        .title(" Memory ")
        .border_color(border)
        .padding(Padding::symmetric(0, 0)),
    )
}

fn conversation_view(
    app: &mut App,
    theme: &Theme,
    transcript_probe: &RectProbe,
    composer_probe: &RectProbe,
    content_width: u16,
) -> Element {
    let lines = timeline_lines(&app.timeline, content_width, theme);
    let viewport_height = if app.selection_area.height == 0 {
        20
    } else {
        usize::from(app.selection_area.height)
    };
    let wrapped_height = tuika::components::text::wrap_lines(&lines, content_width).len();
    app.transcript_content_height = if wrapped_height > viewport_height {
        tuika::components::text::wrap_lines(&lines, content_width.saturating_sub(1)).len()
    } else {
        wrapped_height
    };
    app.transcript_viewport_height = viewport_height;
    app.transcript_scroll
        .clamp(app.transcript_content_height, viewport_height);
    let transcript = transcript_probe.wrap(element(
        Scroll::new(lines, &app.transcript_scroll).wrap(true),
    ));

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
        .fixed(composer_height, composer_probe.wrap(input));
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
    // A result stores only its correlation ID. Resolve that ID against the
    // durable call history so the UI can replace both records with one row.
    let tool_calls = timeline_tool_calls(entries);
    let completed_tool_calls = timeline_completed_tool_calls(entries);
    let mut lines = Vec::new();
    let mut previous_was_tool = false;
    for entry in entries {
        if let TimelineEntry::Message(message) = entry
            && message_only_has_tools(message)
        {
            let before = lines.len();
            append_compact_tools(
                &mut lines,
                message,
                &tool_calls,
                &completed_tool_calls,
                theme,
            );
            previous_was_tool |= lines.len() != before;
            continue;
        }
        if previous_was_tool {
            lines.push(Line::default());
            previous_was_tool = false;
        }
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
                            if !completed_tool_calls.contains(call.id.as_str()) {
                                lines.push(compact_tool_line(
                                    &call.name,
                                    Some(&call.arguments),
                                    None,
                                    None,
                                    false,
                                    theme,
                                ));
                            }
                        }
                        ContentPart::ToolResult(result) => {
                            let call = tool_calls.get(result.tool_call_id.as_str()).copied();
                            lines.push(compact_tool_line(
                                call.map(|(name, _)| name).unwrap_or("tool"),
                                call.map(|(_, arguments)| arguments),
                                result.result.as_ref(),
                                result.error.as_deref(),
                                true,
                                theme,
                            ));
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

type ToolCallIndex<'a> = BTreeMap<&'a str, (&'a str, &'a serde_json::Value)>;

fn timeline_tool_calls(entries: &[TimelineEntry]) -> ToolCallIndex<'_> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            TimelineEntry::Message(message) => Some(message.as_ref()),
            TimelineEntry::Event { .. } => None,
        })
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => {
                Some((call.id.as_str(), (call.name.as_str(), &call.arguments)))
            }
            _ => None,
        })
        .collect()
}

fn timeline_completed_tool_calls(entries: &[TimelineEntry]) -> BTreeSet<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            TimelineEntry::Message(message) => Some(message.as_ref()),
            TimelineEntry::Event { .. } => None,
        })
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn message_only_has_tools(message: &Message) -> bool {
    let mut has_tool = false;
    for part in &message.content {
        match part {
            ContentPart::ToolCall(_) | ContentPart::ToolResult(_) => has_tool = true,
            ContentPart::Text(part) if part.text.trim().is_empty() => {}
            _ => return false,
        }
    }
    has_tool
}

fn append_compact_tools(
    lines: &mut Vec<Line<'static>>,
    message: &Message,
    tool_calls: &ToolCallIndex<'_>,
    completed_tool_calls: &BTreeSet<&str>,
    theme: &Theme,
) {
    for part in &message.content {
        match part {
            ContentPart::ToolCall(call) if !completed_tool_calls.contains(call.id.as_str()) => {
                lines.push(compact_tool_line(
                    &call.name,
                    Some(&call.arguments),
                    None,
                    None,
                    false,
                    theme,
                ));
            }
            ContentPart::ToolResult(result) => {
                let call = tool_calls.get(result.tool_call_id.as_str()).copied();
                lines.push(compact_tool_line(
                    call.map(|(name, _)| name).unwrap_or("tool"),
                    call.map(|(_, arguments)| arguments),
                    result.result.as_ref(),
                    result.error.as_deref(),
                    true,
                    theme,
                ));
            }
            _ => {}
        }
    }
}

fn compact_tool_line(
    name: &str,
    arguments: Option<&serde_json::Value>,
    result: Option<&serde_json::Value>,
    error: Option<&str>,
    completed: bool,
    theme: &Theme,
) -> Line<'static> {
    let (marker, marker_color) = match (completed, error) {
        (_, Some(_)) => ("✗", theme.accent_alt),
        (true, None) => ("✓", theme.code.string),
        (false, None) => ("…", theme.muted),
    };
    let mut spans = vec![
        Span::styled("tool › ", theme.muted_style()),
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::raw(" "),
        Span::styled(
            tool_display_name(name, arguments),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(target) = arguments.and_then(|arguments| tool_target(name, arguments)) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(target, theme.muted_style()));
    }
    let summary = match error {
        Some(error) => Some(format!("error: {}", compact_text(error, 96))),
        None => result.and_then(|result| tool_result_summary(name, result)),
    };
    if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
        spans.push(Span::styled(" · ", theme.muted_style()));
        spans.push(Span::styled(summary, theme.muted_style()));
    }
    Line::from(spans)
}

fn tool_display_name(name: &str, arguments: Option<&serde_json::Value>) -> String {
    if name == "exec"
        && let Some(port) = arguments
            .and_then(|arguments| arguments.get("name"))
            .and_then(serde_json::Value::as_str)
    {
        return compact_text(port, 32);
    }
    if name == "exec"
        && let Some(port) = arguments
            .and_then(|arguments| arguments.get("source"))
            .and_then(serde_json::Value::as_str)
            .and_then(port_name_from_source)
    {
        return compact_text(port, 32);
    }
    compact_text(name, 32)
}

fn tool_target(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let fields = arguments.as_object()?;
    let primary = ["path", "url", "query"]
        .into_iter()
        .find_map(|key| fields.get(key).and_then(serde_json::Value::as_str));
    if name == "exec" {
        let input = fields.get("input")?;
        let input_path = input.get("path").and_then(serde_json::Value::as_str);
        let detail = ["url", "query", "pattern", "filter", "prompt", "task"]
            .into_iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str));
        let method = input.get("method").and_then(serde_json::Value::as_str);
        let mut parts = Vec::new();
        if let Some(path) = fields.get("path").and_then(serde_json::Value::as_str) {
            parts.push(compact_text(path, 36));
        }
        if let Some(method) = method {
            parts.push(compact_text(method, 8));
        }
        if let Some(input_path) = input_path {
            parts.push(compact_text(input_path, 36));
        }
        if let Some(detail) = detail {
            parts.push(compact_text(detail, 48));
        }
        return (!parts.is_empty()).then(|| compact_text(&parts.join(" "), 72));
    }
    if let Some(primary) = primary {
        return Some(compact_text(primary, 72));
    }
    for key in ["pattern", "filter", "prompt"] {
        if let Some(value) = fields.get(key).and_then(serde_json::Value::as_str) {
            return Some(compact_text(value, 72));
        }
    }
    None
}

fn port_name_from_source(source: &str) -> Option<&str> {
    source
        .split_once("(port-call \"")?
        .1
        .split_once('"')
        .map(|(name, _)| name)
}

fn tool_result_summary(name: &str, result: &serde_json::Value) -> Option<String> {
    let fields = result.as_object();
    match name {
        "discover" => fields
            .and_then(|fields| fields.get("entries"))
            .and_then(serde_json::Value::as_array)
            .map(|entries| plural(entries.len(), "entry", "entries")),
        "read" => fields
            .and_then(|fields| fields.get("value"))
            .map(compact_json_summary),
        "stat" => fields
            .and_then(|fields| fields.get("value"))
            .map(compact_stat_summary),
        "write" | "remove" => fields
            .and_then(|fields| fields.get("version"))
            .and_then(serde_json::Value::as_u64)
            .map(|version| format!("v{version}")),
        "exec" => Some(compact_json_summary(result)),
        _ => Some(compact_json_summary(result)),
    }
}

fn compact_stat_summary(value: &serde_json::Value) -> String {
    let Some(fields) = value.as_object() else {
        return compact_json_summary(value);
    };
    let mut parts = ["kind", "locality"]
        .into_iter()
        .filter_map(|key| fields.get(key).and_then(serde_json::Value::as_str))
        .map(|value| compact_text(value, 32))
        .collect::<Vec<_>>();
    if let Some(content) = fields
        .get("facts")
        .and_then(serde_json::Value::as_object)
        .and_then(|facts| facts.get("content"))
        .and_then(serde_json::Value::as_str)
    {
        parts.push(compact_text(content, 32));
    }
    if parts.is_empty() {
        compact_json_summary(value)
    } else {
        parts.join(" · ")
    }
}

fn compact_json_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => {
            let trimmed = value.trim();
            if matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'['))
                && let Ok(parsed) = serde_json::from_str(trimmed)
            {
                return compact_json_summary(&parsed);
            }
            compact_text(value, 80)
        }
        serde_json::Value::Array(values) => plural(values.len(), "item", "items"),
        serde_json::Value::Object(fields) => {
            if fields.contains_key("body")
                && let Some(status) = fields.get("status").and_then(serde_json::Value::as_u64)
            {
                return format!("HTTP {status}");
            }
            for (key, singular, plural_name) in [
                ("entries", "entry", "entries"),
                ("matches", "match", "matches"),
                ("values", "value", "values"),
                ("changed", "path", "paths"),
            ] {
                if let Some(values) = fields.get(key).and_then(serde_json::Value::as_array) {
                    return plural(values.len(), singular, plural_name);
                }
            }
            for key in ["output", "value", "result"] {
                if let Some(value) = fields.get(key) {
                    return compact_json_summary(value);
                }
            }
            for key in ["id", "response"] {
                if let Some(value) = fields.get(key).and_then(serde_json::Value::as_str) {
                    return compact_text(value, 80);
                }
            }
            if let Some(version) = fields.get("version").and_then(serde_json::Value::as_u64) {
                return format!("v{version}");
            }
            if let Some(messages) = fields.get("messages").and_then(serde_json::Value::as_array) {
                return plural(messages.len(), "message", "messages");
            }
            plural(fields.len(), "field", "fields")
        }
    }
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let mut compact = String::new();
    let mut whitespace = false;
    let mut truncated = false;
    for character in text.chars() {
        if character.is_whitespace() {
            whitespace = !compact.is_empty();
            continue;
        }
        if whitespace {
            if compact.chars().count() >= max_chars.saturating_sub(1) {
                truncated = true;
                break;
            }
            compact.push(' ');
            whitespace = false;
        }
        if compact.chars().count() >= max_chars.saturating_sub(1) {
            truncated = true;
            break;
        }
        compact.push(character);
    }
    if truncated {
        compact.push('…');
    }
    compact
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
    use everruns_test_support::{LLMSIM_MODEL_ID, LlmSimConfig, llm_sim_provider};
    use svit::{Mount, Reasoner, value};
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
            let (kind, content, entries) = match value {
                Value::Map(values) => ("directory", "object", Some(values.len())),
                Value::Array(values) => ("directory", "array", Some(values.len())),
                Value::String(_) => ("leaf", "text/plain", None),
                Value::Script(_) => ("leaf", "svit-script", None),
                _ => ("leaf", "scalar", None),
            };
            let locality = self
                .localities
                .get(path)
                .cloned()
                .unwrap_or_else(|| "cache".into());
            let mut facts = BTreeMap::from([("content".into(), Value::from(content))]);
            if let Some(entries) = entries {
                facts.insert("entries".into(), Value::Integer(entries as i64));
            }
            Ok(Some(Value::Map(BTreeMap::from([
                ("kind".into(), Value::from(kind)),
                ("facts".into(), Value::Map(facts)),
                ("hash".into(), Value::from(value.content_hash())),
                ("locality".into(), Value::from(locality)),
                ("path".into(), Value::from(path)),
            ]))))
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
            assert!(
                self.app.rows.iter().any(|row| row.id == path),
                "no row for {path}"
            );
            self.app.tree.select(Some(path.into()));
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
        for path in paths {
            app.tree.expand((*path).into());
        }
        app.app.resolve(&app.view);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
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
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"/memory"));
        assert!(paths.contains(&"/mounts"));
        assert!(paths.contains(&"/system"));

        app.tree.expand("/thread".into());
        app.resolve(&svit);
        let process_id_label = &app
            .rows
            .iter()
            .find(|row| row.id == "/thread/process_id")
            .expect("Svit initialization exposes bounded thread metadata")
            .label;
        assert!(
            line_text(process_id_label).contains("process_id"),
            "{}",
            line_text(process_id_label)
        );

        app.tree.expand("/mounts".into());
        app.tree.expand("/mounts/files".into());
        app.tree.expand("/mounts/files/notes".into());
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
            .find(|row| row.id == "/mounts/files/greeting.txt")
            .unwrap()
            .label
            .clone();
        assert!(
            line_text(&label).contains("greeting.txt (local)"),
            "{label:?}"
        );
        assert_eq!(app.nodes.value("/mounts/files/greeting.txt"), None);

        // Selecting it reads the real file.
        let index = app
            .rows
            .iter()
            .find(|row| row.id == "/mounts/files/greeting.txt")
            .map(|row| row.id.clone())
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
        app.tree.expand("/mounts".into());
        app.tree.expand("/mounts/files".into());
        app.resolve(&svit);
        assert_eq!(app.nodes.children("/mounts/files").len(), 2);

        // Building a Svit commits its thread and port catalog, so the
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
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["/"]
        );

        app.resolve(&view);

        // Resolving the open root lists its children and nothing deeper.
        let paths = app
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["/", "/memory", "/mounts"]);

        app.tree.expand("/mounts".into());
        app.tree.expand("/mounts/cwd".into());
        app.resolve(&view);

        // A mount is opened through the same discover/stat path as memory.
        let paths = app
            .rows
            .iter()
            .map(|row| row.id.as_str())
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
                .find(|row| row.id == path)
                .unwrap()
                .label
                .clone()
        };

        assert!(line_text(&label("/mounts/cwd")).contains("cwd (local)"));
        assert!(line_text(&label("/mounts/cwd/README.md")).contains("README.md (local)"));
        // Resident committed state carries no cost annotation.
        assert!(!line_text(&label("/mounts")).contains('('));
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
        assert_eq!(app.selected().id, "/mounts/cwd/a.txt");
        assert_eq!(app.selected_value(), &Value::from("two"));
    }

    #[test]
    fn a_matching_content_hash_keeps_a_reported_node_resolved() {
        let mut app = test_app(value!({"memory": {"count": 1, "color": "blue"}}), 1);
        expand_paths(&mut app, &["/memory"]);
        app.select("/memory/color");
        let unchanged = app.nodes.value("/memory/color").cloned();

        // The commit reports /memory/color, but its content is what the
        // console already holds, so nothing about it needs reading again.
        let hashes = BTreeMap::from([
            (
                "/memory/color".to_owned(),
                Some(value!("blue").content_hash()),
            ),
            (
                "/memory".to_owned(),
                Some(value!({"count": 2}).content_hash()),
            ),
        ]);
        app.app.refresh_process(&Change::notification_with_hashes(
            2,
            vec!["/memory/color".to_owned()],
            hashes,
        ));

        assert!(!app.nodes.stale_nodes.contains("/memory/color"));
        assert!(!app.nodes.stale_values.contains("/memory/color"));
        assert_eq!(app.nodes.value("/memory/color"), unchanged.as_ref());
        // The ancestor's published hash differs, so its listing is read again.
        assert!(app.nodes.stale_children.contains("/memory"));
    }

    #[test]
    fn a_thread_history_append_is_not_a_process_commit() {
        let mut app = test_app_with(
            ValueView::new(value!({
                "memory": {"release": {"color": "blue"}},
                "thread": {"format": "svit-thread@8"}
            })),
            3,
            "sim",
        );
        expand_paths(&mut app, &["/memory", "/memory/release"]);

        app.push_message(Message::assistant("Stored in memory."));

        // Lampa owns the thread projection, so the committed tree around it
        // keeps both its version and its resolved nodes.
        assert_eq!(app.version, 3);
        assert!(!app.nodes.stale_nodes.contains("/"));
        assert!(!app.nodes.stale_children.contains("/"));
        assert!(!app.nodes.stale_children.contains("/memory"));
        assert!(!app.nodes.stale_values.contains("/memory/release/color"));
        assert!(app.rows.iter().any(|row| row.id == "/memory/release"));
    }

    #[test]
    fn a_commit_never_blanks_the_resolved_tree() {
        let mut app = test_app(
            value!({"memory": {"count": 1}, "thread": {"format": "x"}}),
            1,
        );
        expand_paths(&mut app, &["/memory"]);

        // Every change reaches the root, so a discarding invalidation would
        // leave one row until the next resolution paints the tree again.
        app.app.refresh_process(&Change::notification(
            2,
            vec!["/thread/events".to_owned(), "/thread/messages".to_owned()],
        ));

        assert!(app.rows.iter().any(|row| row.id == "/memory"));
        assert!(app.rows.iter().any(|row| row.id == "/memory/count"));
        assert!(app.rows.iter().any(|row| row.id == "/thread"));
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
        // The changed path and its parent listing are the only entries the
        // next resolution has to read again.
        assert!(app.nodes.stale_nodes.contains("/memory/count"));
        assert!(app.nodes.stale_children.contains("/memory"));
        assert!(!app.nodes.stale_children.contains("/mounts/cwd"));
        assert!(!app.nodes.stale_values.contains("/mounts/cwd/a.txt"));

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
        assert_eq!(app.selected().id, "/mounts/cwd/a.txt");
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
                .filter(|row| row.id.starts_with("/memory/file-"))
                .count(),
            MAX_TREE_CHILDREN
        );
        assert!(
            app.rows
                .iter()
                .any(|row| line_text(&row.label).contains("first 200 entries"))
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

        assert_eq!(app.tree.expanded().collect::<Vec<_>>(), [&"/".to_owned()]);
        assert!(app.rows.iter().any(|row| row.id == "/memory"));
        assert!(!app.rows.iter().any(|row| row.id == "/memory/profile"));
    }

    #[test]
    fn bounded_thread_history_is_browseable_without_entering_process_state() {
        let view = ValueView::new(value!({
            "thread": {"format": "svit-thread@8"},
            "memory": {}
        }));
        let mut app = App::new(0, "test".into());
        app.set_thread_events(vec![value!({
            "type": "input.message",
            "data": {"text": "remember this"}
        })]);
        app.push_message(Message::user("remember this"));
        app.tree.expand("/".into());
        app.tree.expand("/thread".into());
        app.tree.expand("/thread/events".into());
        app.tree.expand("/thread/messages".into());
        let history = Arc::clone(&app.thread_history);
        let history_view = ThreadHistoryView::new(&view, &history);
        app.resolve(&history_view);

        let paths = app
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"/thread/events"));
        assert!(paths.contains(&"/thread/events/0"));
        assert!(paths.contains(&"/thread/messages"));
        assert!(paths.contains(&"/thread/messages/0"));
        assert_eq!(view.at("/thread/events"), None);

        app.tree.select(Some("/thread/events/0".into()));
        app.resolve(&history_view);
        assert_eq!(
            app.selected_value().to_json()["type"].as_str(),
            Some("input.message")
        );
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
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        expand_paths(
            &mut app,
            &all.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        expand_paths(&mut app, &["/items"]);
        let label = |path: &str| {
            app.rows
                .iter()
                .find(|row| row.id == path)
                .unwrap()
                .label
                .clone()
        };

        assert!(line_text(&label("/items/0")).ends_with("[0] - plain text"));
        assert!(line_text(&label("/items/1")).ends_with("[1] - 7"));
        assert!(line_text(&label("/items/2")).ends_with("[2] - name: Ada"));
        assert!(line_text(&label("/items/3")).ends_with("[3] - count: 2"));
        assert!(line_text(&label("/items/4")).ends_with("[4] - object · 1 item"));
        assert!(line_text(&label("/items/5")).contains("[5] - start "));
        assert!(line_text(&label("/items/5")).ends_with('…'));
        assert!(!line_text(&label("/items/5")).contains('\n'));
        assert!(!line_text(&label("/items/5")).contains('\u{1b}'));
    }

    #[test]
    fn tree_flattens_complete_process_memory_and_preview_tracks_selection() {
        let mut app = test_app_with(
            ValueView::new(value!({
                "thread": {"format": "svit-thread@8"},
                "memory": {"profile": {"name": "Ada"}, "scores": [3, 5]}
            })),
            7,
            "test-model",
        );

        assert_eq!(line_text(&app.rows[0].label), "/");
        assert!(app.rows.iter().any(|row| row.id == "/memory"));
        assert!(!app.rows.iter().any(|row| row.id == "/thread/format"));
        expand_paths(
            &mut app,
            &["/thread", "/memory", "/memory/profile", "/memory/scores"],
        );
        assert!(app.rows.iter().any(|row| row.id == "/thread/format"));
        assert!(
            app.rows
                .iter()
                .any(|row| line_text(&row.label).ends_with("name"))
        );
        app.select("/memory/profile/name");

        assert_eq!(app.selected().id, "/memory/profile/name");
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
    fn command_click_resolves_a_transcript_url() {
        let mut app = test_app(value!({}), 0);
        app.app
            .push_message(Message::assistant("Visit https://everruns.com/ now."));
        let probes = layout(&mut app, 90, 24);
        let theme = lampa_theme();
        let root = build_view(&mut app.app, Rect::new(0, 0, 90, 24), &theme, &probes);
        let buffer = render(root.as_ref(), 90, 24, &theme);
        let frame = grid(&buffer);
        let (row, column) = frame
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                line.find("https://everruns.com/")
                    .map(|column| (row as u16, column as u16))
            })
            .expect("transcript URL is visible");
        let mut click = Mouse::at(MouseKind::Up(MouseButton::Left), column, row);
        click.super_key = true;

        assert_eq!(
            transcript_link_click(&click, &buffer, probes.transcript.rect()),
            Some("https://everruns.com/".into())
        );
    }

    #[test]
    fn timeline_projects_llm_commentary_and_tool_calls_from_svit_events() {
        let optimistic_user = Message::user("Inspect memory.");
        let canonical_user = Message::user("Inspect memory.");
        assert_ne!(optimistic_user.id, canonical_user.id);
        let mut commentary = serde_json::to_value(Message::assistant(
            "I’ll inspect the memory tree before continuing.",
        ))
        .unwrap();
        commentary["phase"] = serde_json::Value::String("commentary".into());

        let mut tool_call = Message::assistant("");
        tool_call.content = vec![ContentPart::tool_call(
            "call_1",
            "discover",
            serde_json::json!({"path": "/memory"}),
        )];
        let mut app = App::new(0, "test".into());
        app.push_user(optimistic_user);
        app.push_message(canonical_user);
        app.push_message(serde_json::from_value(commentary).unwrap());
        app.push_message(tool_call);

        let lines = timeline_lines(&app.timeline, 80, &lampa_theme());
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("inspect the memory tree"));
        assert!(rendered.contains("tool › … discover /memory"));
        assert_eq!(
            app.timeline.len(),
            3,
            "projection and optimistic inbox display must not duplicate messages"
        );
    }

    #[test]
    fn completed_tool_call_is_one_helpful_row_without_its_internal_id() {
        let mut call = Message::assistant("");
        call.content = vec![ContentPart::tool_call(
            "call_internal_123",
            "discover",
            serde_json::json!({"path": "/memory"}),
        )];
        let entries = vec![
            TimelineEntry::Message(Box::new(call)),
            TimelineEntry::Message(Box::new(Message::tool_result(
                "call_internal_123",
                Some(serde_json::json!({
                    "path": "/memory",
                    "entries": ["profile", "research"]
                })),
                None,
            ))),
        ];

        let lines = timeline_lines(&entries, 80, &lampa_theme());
        let nonempty = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();

        assert_eq!(nonempty, ["tool › ✓ discover /memory · 2 entries"]);
        assert!(!nonempty.join(" ").contains("call_internal_123"));
    }

    #[test]
    fn compact_inline_port_exec_identifies_the_target_and_outcome() {
        let mut call = Message::assistant("");
        call.content = vec![ContentPart::tool_call(
            "call_http",
            "exec",
            serde_json::json!({
                "source": "(define (main input) (port-call \"http\" input))",
                "input": {"method": "GET", "url": "https://everruns.com/"}
            }),
        )];
        let entries = vec![
            TimelineEntry::Message(Box::new(call)),
            TimelineEntry::Message(Box::new(Message::tool_result(
                "call_http",
                Some(serde_json::json!(
                    r#"{"status":200,"headers":{},"body":"ok"}"#
                )),
                None,
            ))),
        ];

        let rendered = timeline_lines(&entries, 100, &lampa_theme())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("tool › ✓ http GET https://everruns.com/ · HTTP 200"));
        assert!(!rendered.contains("call_http"));
    }

    #[test]
    fn compact_tool_failure_shows_the_error_instead_of_the_call_id() {
        let mut call = Message::assistant("");
        call.content = vec![ContentPart::tool_call(
            "call_failed",
            "read",
            serde_json::json!({"path": "/memory/missing"}),
        )];
        let entries = vec![
            TimelineEntry::Message(Box::new(call)),
            TimelineEntry::Message(Box::new(Message::tool_result(
                "call_failed",
                None,
                Some("path not found".into()),
            ))),
        ];

        let rendered = timeline_lines(&entries, 80, &lampa_theme())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("tool › ✗ read /memory/missing · error: path not found"));
        assert!(!rendered.contains("call_failed"));
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
        let rendered = preview_lines("/memory/event", &event, 80, &theme)
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
        app.tree.select(Some("/memory/profile".into()));

        app.refresh_process(
            value!({
                "thread": {"messages": []},
                "memory": {"profile": {"name": "Grace"}, "z": true}
            }),
            2,
        );

        assert_eq!(app.selected().id, "/memory/profile");
        assert_eq!(app.version, 2);
    }

    #[test]
    fn batched_commit_refreshes_preserve_the_original_selection() {
        let mut app = test_app(
            value!({
                "inbox": [],
                "memory": {"profile": {"name": "Ada"}},
                "thread": {"format": "svit-thread@8"}
            }),
            1,
        );
        expand_paths(&mut app, &["/memory", "/memory/profile"]);
        app.select("/memory/profile/name");

        app.app
            .refresh_process(&Change::notification(2, vec!["/inbox".to_owned()]));
        app.app
            .refresh_process(&Change::notification(3, vec!["/thread".to_owned()]));
        app.app.resolve(&app.view);

        assert_eq!(app.selected().id, "/memory/profile/name");
        assert!(app.tree.is_expanded(&"/memory".into()));
        assert!(app.tree.is_expanded(&"/memory/profile".into()));
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
        app.tree.select(Some("/memory".into()));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Down)));

        assert_eq!(app.selected().id, "/thread");
        assert_eq!(app.preview_scroll.offset(), 0);
    }

    #[test]
    fn memory_nodes_collapse_expand_and_stay_collapsed_after_refresh() {
        let mut app = test_app(
            value!({"thread": {"format": "svit-thread@8"}, "memory": {}}),
            0,
        );
        app.focus = Focus::Memory;
        app.tree.select(Some("/thread".into()));

        assert!(!app.tree.is_expanded(&"/thread".into()));
        assert!(!app.rows.iter().any(|row| row.id == "/thread/format"));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert!(app.tree.is_expanded(&"/thread".into()));
        assert!(app.rows.iter().any(|row| row.id == "/thread/format"));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Enter)));

        assert!(!app.tree.is_expanded(&"/thread".into()));
        assert!(app.rows.iter().any(|row| row.id == "/thread/format"));
        app.refresh_process(
            value!({"thread": {"format": "svit-thread@8"}, "memory": {}}),
            1,
        );
        assert!(!app.tree.is_expanded(&"/thread".into()));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Right)));

        assert_eq!(app.selected().id, "/thread");
        assert!(app.tree.is_expanded(&"/thread".into()));
        assert!(app.rows.iter().any(|row| row.id == "/thread/format"));
    }

    #[test]
    fn left_on_a_closed_node_moves_to_its_parent() {
        let mut app = test_app(value!({"memory": {"profile": {"name": "Ada"}}}), 0);
        app.focus = Focus::Memory;
        expand_paths(&mut app, &["/memory", "/memory/profile"]);
        app.tree.select(Some("/memory/profile".into()));
        let _ = app.handle(&Event::Key(Key::new(KeyCode::Left)));
        assert_eq!(app.selected().id, "/memory/profile");
        assert!(!app.tree.is_expanded(&"/memory/profile".into()));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::Left)));

        assert_eq!(app.selected().id, "/memory");
    }

    #[test]
    fn memory_tree_pages_and_scrolls_with_the_mouse_wheel() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        app.focus = Focus::Memory;
        app.memory_viewport_height = 4;
        expand_paths(&mut app, &["/memory"]);

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageDown)));
        assert_eq!(app.tree.selected().map(String::as_str), Some("/memory/1"));

        let _ = app.handle(&Event::Mouse(Mouse::at(MouseKind::ScrollDown, 0, 0)));
        assert_eq!(app.tree.selected().map(String::as_str), Some("/memory/4"));

        let _ = app.handle(&Event::Key(Key::new(KeyCode::PageUp)));
        assert_eq!(app.tree.selected().map(String::as_str), Some("/memory/1"));
    }

    #[test]
    fn mouse_wheel_over_memory_routes_to_memory_without_keyboard_focus() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        app.memory_viewport_height = 4;
        expand_paths(&mut app, &["/memory"]);
        let probes = layout(&mut app, 90, 24);

        let _ = app.handle(&panel_mouse(MouseKind::ScrollDown, probes.memory.rect()));

        assert_eq!(app.focus, Focus::Memory);
        assert_eq!(app.tree.selected().map(String::as_str), Some("/memory/1"));
    }

    #[test]
    fn transcript_wheel_direction_uses_its_wrapped_height() {
        let mut app = test_app(value!({}), 0);
        app.app
            .push_message(Message::assistant("word ".repeat(600)));
        let probes = layout(&mut app, 90, 24);
        let transcript = probes.transcript.rect();
        let bottom = app.app.transcript_scroll.offset();
        assert!(bottom > 3, "the transcript must overflow the viewport");

        let _ = app.app.handle(&Event::Mouse(Mouse::at(
            MouseKind::ScrollUp,
            transcript.x,
            transcript.y,
        )));
        assert!(app.app.transcript_scroll.offset() < bottom);

        let _ = app.app.handle(&Event::Mouse(Mouse::at(
            MouseKind::ScrollDown,
            transcript.x,
            transcript.y,
        )));
        assert_eq!(app.app.transcript_scroll.offset(), bottom);
    }

    #[test]
    fn transcript_wheel_reversal_updates_the_next_rendered_frame() {
        let mut app = test_app(value!({}), 0);
        for index in 0..12 {
            app.app.push_message(Message::assistant(format!(
                "message-{index:02} {}",
                "word ".repeat(80)
            )));
        }
        let theme = lampa_theme();
        let draw = |app: &mut TestApp| {
            let probes = layout(app, 90, 24);
            let root = build_view(&mut app.app, Rect::new(0, 0, 90, 24), &theme, &probes);
            let frame = grid(&render(root.as_ref(), 90, 24, &theme));
            (probes, frame)
        };

        let (probes, bottom) = draw(&mut app);
        let transcript = probes.transcript.rect();
        let _ = app.app.handle(&Event::Mouse(Mouse::at(
            MouseKind::ScrollUp,
            transcript.x,
            transcript.y,
        )));
        let (_, after_up) = draw(&mut app);
        assert_ne!(after_up, bottom, "scroll up must change the next frame");

        let _ = app.app.handle(&Event::Mouse(Mouse::at(
            MouseKind::ScrollDown,
            transcript.x,
            transcript.y,
        )));
        let (_, after_down) = draw(&mut app);
        assert_eq!(after_down, bottom, "scroll reversal must apply immediately");
    }

    #[test]
    fn ready_terminal_events_drains_a_queued_wheel_reversal_in_one_frame() {
        let wheel = |kind| {
            event::Event::Mouse(event::MouseEvent {
                kind,
                column: 50,
                row: 10,
                modifiers: event::KeyModifiers::NONE,
            })
        };
        let first = wheel(event::MouseEventKind::ScrollUp);
        let mut readiness = [true, true, false].into_iter();
        let mut remaining = [
            wheel(event::MouseEventKind::ScrollUp),
            wheel(event::MouseEventKind::ScrollDown),
        ]
        .into_iter();

        let events = ready_terminal_events(
            first,
            || Ok(readiness.next().unwrap_or(false)),
            || Ok(remaining.next().expect("ready event must be readable")),
        )
        .expect("pending terminal input should drain");

        assert_eq!(
            events,
            [
                wheel(event::MouseEventKind::ScrollUp),
                wheel(event::MouseEventKind::ScrollUp),
                wheel(event::MouseEventKind::ScrollDown),
            ]
        );
    }

    #[test]
    fn ready_terminal_events_has_a_per_frame_limit() {
        let first = event::Event::Resize(80, 24);
        let mut read_count = 0_u16;

        let events = ready_terminal_events(
            first,
            || Ok(true),
            || {
                read_count += 1;
                Ok(event::Event::Resize(80 + read_count, 24))
            },
        )
        .expect("continuous terminal input should be bounded");

        assert_eq!(events.len(), MAX_READY_INPUT_EVENTS);
        assert_eq!(read_count as usize, MAX_READY_INPUT_EVENTS - 1);
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
    fn dragging_transcript_text_is_consumed_without_changing_the_memory_tree() {
        let mut app = test_app(value!({"memory": {"alpha": 1}}), 0);
        app.app
            .push_message(Message::assistant("hello selectable world"));
        let probes = layout(&mut app, 90, 24);
        let transcript = probes.transcript.rect();
        let selected = app.app.tree.selected().cloned();

        let down = Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            transcript.x,
            transcript.y + 1,
        ));
        let drag = Event::Mouse(Mouse::at(
            MouseKind::Drag(MouseButton::Left),
            transcript.x + 4,
            transcript.y + 1,
        ));
        let up = Event::Mouse(Mouse::at(
            MouseKind::Up(MouseButton::Left),
            transcript.x + 4,
            transcript.y + 1,
        ));

        assert!(app.app.route_mouse(&down));
        assert!(app.app.route_mouse(&drag));
        assert!(app.app.route_mouse(&up));
        assert_eq!(app.app.tree.selected(), selected.as_ref());

        let theme = lampa_theme();
        let root = build_view(&mut app.app, Rect::new(0, 0, 90, 24), &theme, &probes);
        let mut buffer = render(root.as_ref(), 90, 24, &theme);
        app.app.capture_panel_bounds(&probes);
        assert_eq!(
            app.app.finish_selection_frame(&mut buffer, &theme),
            Some("hello".into())
        );
        for column in transcript.x..=transcript.x + 4 {
            assert_eq!(buffer[(column, transcript.y + 1)].bg, theme.selection_bg);
        }
    }

    #[test]
    fn ctrl_c_copies_an_active_selection_instead_of_quitting() {
        let mut app = test_app(value!({}), 0);
        app.app.push_message(Message::assistant("select me"));
        let probes = layout(&mut app, 90, 24);
        let transcript = probes.transcript.rect();
        for kind in [
            MouseKind::Down(MouseButton::Left),
            MouseKind::Drag(MouseButton::Left),
            MouseKind::Up(MouseButton::Left),
        ] {
            let column = if matches!(kind, MouseKind::Down(_)) {
                transcript.x
            } else {
                transcript.x + 5
            };
            let _ = app
                .app
                .route_mouse(&Event::Mouse(Mouse::at(kind, column, transcript.y + 1)));
        }
        let mut ctrl_c = Key::new(KeyCode::Char('c'));
        ctrl_c.ctrl = true;

        assert_eq!(app.app.handle(&Event::Key(ctrl_c)), AppAction::Continue);
        assert!(app.app.pending_copy);
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
        assert_eq!(app.selected().id, "/memory/alpha");
    }

    #[test]
    fn clicking_a_scrolled_memory_row_uses_the_visible_offset() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        app.tree.select(Some("/memory/8".into()));
        let probes = layout(&mut app, 90, 8);
        let first_visible = app.memory_window.start();
        let expected_path = app.rows[first_visible].id.clone();
        let bounds = probes.memory.rect();

        let _ = app.handle(&Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            bounds.x + 2,
            bounds.y + 1,
        )));

        assert_eq!(app.selected().id, expected_path);
    }

    #[test]
    fn clicking_the_bottom_row_keeps_the_memory_window_stable() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        app.tree.select(Some("/memory/5".into()));
        let probes = layout(&mut app, 90, 8);
        let first_visible = app.memory_window.start();
        let body = panel_body(probes.memory.rect());

        let _ = app.handle(&Event::Mouse(Mouse::at(
            MouseKind::Down(MouseButton::Left),
            body.x + 1,
            body.bottom() - 1,
        )));

        assert_eq!(app.memory_window.start(), first_visible);
    }

    #[test]
    fn memory_tree_viewport_follows_an_offscreen_selection() {
        let mut app = test_app(value!({"memory": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}), 0);
        expand_paths(&mut app, &["/memory"]);
        app.memory_viewport_height = 4;
        app.tree.select(Some("/memory/8".into()));
        app.reconcile_tree();
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
        app.push_user(Message::user("Remember the release color."));
        app.push_message(Message::assistant("Stored in memory."));
        app.finish_turn();
        let theme = lampa_theme();
        let probes = UiProbes::default();
        let root = build_view(&mut app, Rect::new(0, 0, 90, 24), &theme, &probes);

        let frame = grid(&render(root.as_ref(), 90, 24, &theme));

        assert!(!frame.contains("Inbox / Outbox"));
        assert!(frame.contains(" Memory "));
        // A commit no longer blanks the tree, so both top-level sections are
        // on screen in this viewport.
        assert!(frame.contains("thread"));
        assert!(frame.contains("memory"));
        assert!(frame.contains("Stored in memory."));
        assert!(frame.contains("v3 · sim · ready"));
    }

    #[test]
    fn exit_banner_is_centered_and_uses_yolop_colours() {
        let banner = centered_ukraine_banner(40);

        assert!(banner.starts_with("   \x1b[38;2;45;91;158m"));
        assert!(banner.contains("Світ, це Світ. Зроблено в "));
        assert!(banner.contains("\x1b[38;2;126;94;19mУкраїні\x1b[0m"));
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
