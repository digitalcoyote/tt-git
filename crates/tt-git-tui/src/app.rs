//! Top-level application state, update and draw.

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tt_git_core::branches::matches_filter;
use tt_git_core::time::SharedClock;
use tt_git_core::{
    CheckoutPlan, CommitFile, FileState, Head, LogFilter, NoStep, Repo, Stash, StatusEntry, UNDO_CHANGES_KEPT, UndoStep,
};

use crate::action::{Action, RefsSnapshot};
use crate::component::{Component, EventState};
use crate::config::Config;
use crate::editor::{self, EditTarget, EditorLaunch};
use crate::filter::FilterPanel;
use crate::jobs::{Ctx, JobTracker, LatestJob, Reload};
use crate::keymap::{Command, Context, KeyCombo, Keymap, KeymapError};
use crate::layout::View;
use crate::loaders;
use crate::modal::{GitOp, Modal, ModalOutcome};
use crate::panes::{
    BlameView, BranchFocus, BranchTab, BranchesPane, DetailsPane, DetailsSubject, GraphPane, LoadToken, PaneId,
    SavedDetails, StatusPane,
};
use crate::theme::Theme;
use crate::watcher::RepoWatcher;
use crate::widgets::truncate_spans;

mod agent;
mod panes;
mod plugins;
pub use panes::PaneRequest;
pub use plugins::PluginOptions;

/// The longest gap between the clicks of a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Worktree folders walked at once for their sizes. Walking all 22 of a big
/// repo's worktrees together made git commands 2-5x slower until they were
/// done (the graph lagged as you moved through the list); 4 left git
/// unaffected and still finishes in seconds.
const SIZE_JOBS: usize = 4;

/// Commands always listed in the help bar after the focused pane's own.
const GLOBAL_HELP: &[Command] = &[Command::FocusNext, Command::Help, Command::Quit];

/// Global commands listed in the help popup.
const ALL_GLOBAL: &[Command] = &[
    Command::FocusGraph,
    Command::FocusStatus,
    Command::FocusBranches,
    Command::FocusDetails,
    Command::FocusNext,
    Command::FocusPrev,
    Command::FocusLeft,
    Command::FocusDown,
    Command::FocusUp,
    Command::FocusRight,
    Command::Zoom,
    Command::MoveUp,
    Command::MoveDown,
    Command::PageUp,
    Command::PageDown,
    Command::Top,
    Command::Bottom,
    Command::Refresh,
    Command::Fetch,
    Command::Pull,
    Command::Push,
    Command::Undo,
    Command::Help,
    Command::Quit,
];

/// Ticks of [`crate::runner::TICK`] in a second.
const TICKS_PER_SEC: u64 = 1000 / crate::runner::TICK.as_millis() as u64;

/// The spinner beside a running git operation, one frame per tick.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long a notice shows next to a running operation before only the
/// operation is left.
const NOTICE_HOLD: u64 = 3 * TICKS_PER_SEC;

/// How long a `✓` notice stays green before it fades to muted.
const SUCCESS_FADE: u64 = 5 * TICKS_PER_SEC;

/// A git write in flight, shown in the help bar until it's done.
#[derive(Debug)]
struct Busy {
    /// The op's [`GitOp::label`], to find it again when it's done.
    label: String,
    /// What the help bar says while it runs: `pushing main → origin/main`.
    doing: String,
    /// What it says once it's done: `pushed main → origin/main`.
    done: String,
    /// [`App::ticks`] when it started.
    since: u64,
}

/// A git load for the details pane's subject.
#[derive(Debug, Clone, PartialEq)]
enum DetailsLoad {
    Commit(String),
    CommitFiles(String),
    File(StatusEntry),
    Stage(StatusEntry),
    Stash(Stash),
}

impl DetailsLoad {
    fn subject(&self) -> DetailsSubject {
        match self {
            DetailsLoad::Commit(oid) => DetailsSubject::Commit(oid.clone()),
            DetailsLoad::CommitFiles(oid) => DetailsSubject::CommitFiles(oid.clone()),
            DetailsLoad::File(entry) => DetailsSubject::File(entry.path.clone()),
            DetailsLoad::Stage(entry) => DetailsSubject::Stage(entry.path.clone()),
            DetailsLoad::Stash(stash) => DetailsSubject::Stash(stash.oid.clone()),
        }
    }

    /// Index into [`App::details_jobs`]: one at a time of each kind.
    fn kind(&self) -> usize {
        match self {
            DetailsLoad::Commit(_) => 0,
            DetailsLoad::CommitFiles(_) => 1,
            DetailsLoad::File(_) => 2,
            DetailsLoad::Stage(_) => 3,
            DetailsLoad::Stash(_) => 4,
        }
    }
}

/// A focused branch's history load, waiting for or holding its slot.
#[derive(Debug, Clone)]
struct HistoryLoad {
    load: LoadToken,
    focus: BranchFocus,
    limit: usize,
}

impl PartialEq for HistoryLoad {
    fn eq(&self, other: &Self) -> bool {
        self.load.generation() == other.load.generation()
    }
}

/// Where `esc` goes back to from a view opened on top of another (blame,
/// or the commit opened from a blamed line): the details shown before, and
/// the focus and layout they were shown with.
#[derive(Debug)]
struct Back {
    details: SavedDetails,
    /// How the view on top names it, after its title.
    label: String,
    focus: PaneId,
    zoom: Zoom,
    details_open: bool,
}

/// How much of the screen the graph and details panes take. `+` toggles
/// between [`Zoom::Off`] and [`Zoom::Full`]; `space` in a fullscreen graph
/// opens details below it in [`Zoom::Wide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zoom {
    #[default]
    Off,
    /// Only graph and details (if it's open), at full width.
    Wide,
    /// One pane fills the screen.
    Full(PaneId),
}

pub struct App {
    ctx: Ctx,
    config: Config,
    keymap: Keymap,
    theme: Theme,
    repo: Repo,
    focus: PaneId,
    zoom: Zoom,
    /// Details is open in place of the layout's `details = true` node.
    details_open: bool,
    status: StatusPane,
    branches: BranchesPane,
    graph: GraphPane,
    details: DetailsPane,
    /// HEAD as of the last refs load.
    head: Option<Head>,
    clock: SharedClock,
    graph_limit: usize,
    modal: Option<Modal>,
    /// Write operations in flight, oldest first.
    busy: Vec<Busy>,
    /// Ticks since the app started, for the spinner and how long things
    /// have been showing.
    ticks: u64,
    /// The notice last drawn and the tick it was first drawn at.
    notice_drawn: (Option<String>, u64),
    /// Select HEAD in the graph once refs reload (after commit/checkout).
    select_head: bool,
    /// An undo is being worked out or run; another press waits.
    undo_pending: bool,
    /// A check for a tt-git update is running.
    update_checking: bool,
    /// Worktree changes are being counted, and whether to count again
    /// when that's done (something changed meanwhile).
    worktree_count: (bool, bool),
    /// Worktree sizes being measured right now.
    worktree_sizing: usize,
    /// Worktrees waiting for a measuring job, last first, each with the
    /// worktrees nested in it to leave out.
    worktree_size_queue: Vec<(std::path::PathBuf, Vec<std::path::PathBuf>)>,
    /// The worktrees whose sizes were last measured. Walking a folder can
    /// be slow, so a refresh measures again only when this list changes.
    worktree_sized: Vec<std::path::PathBuf>,
    /// The worktree to open in place of this repository once the app quits.
    reopen: Option<Repo>,
    watcher: Option<RepoWatcher>,
    plugins: Option<plugins::PluginState>,
    panes: Vec<crate::pty_pane::PtyPane>,
    next_pane_id: u64,
    release_keys: Vec<KeyCombo>,
    /// (main, details) areas from the last frame, for sizing new panes.
    layout_rects: (Rect, Rect),
    show_help: bool,
    /// What's typed into the help popup to narrow its list.
    help_query: String,
    error: Option<String>,
    notice: Option<String>,
    /// Text waiting for the runner to put on the clipboard.
    clipboard: Option<String>,
    /// An editor waiting for the runner to start.
    editor: Option<EditorLaunch>,
    should_quit: bool,
    pane_areas: Vec<(PaneId, Rect)>,
    /// The last left click on a pane's body: when, and which pane and row,
    /// to spot a double click.
    last_click: Option<(Instant, PaneId, usize)>,
    /// Something on screen may have changed since the last frame.
    dirty: bool,
    /// The last frame showed the terminal cursor (in a focused plugin pane).
    /// Every frame re-sends show-cursor and its position, so such frames keep
    /// coming on ticks, as they always have.
    cursor_shown: bool,
    /// The load the details subject still needs (and whether it must rerun
    /// even if that load is running), started once details is on screen.
    details_wanted: Option<(DetailsLoad, bool)>,
    /// Details loads by [`DetailsLoad::kind`].
    details_jobs: [LatestJob<DetailsLoad>; 5],
    /// Diffs for the details files view.
    file_diff_job: LatestJob<(String, CommitFile)>,
    /// Branch histories for the graph's focused branch.
    history_job: LatestJob<HistoryLoad>,
    /// Views to go back to with `esc`, the latest last. Only while details
    /// has the focus.
    back: Vec<Back>,
    refs_reload: Reload,
    status_reload: Reload,
}

impl App {
    /// Creates the app and the receiving end of its action channel. Call
    /// [`start`](Self::start) inside a tokio runtime to begin loading.
    pub fn new(
        config: Config,
        repo: Repo,
        clock: SharedClock,
    ) -> Result<(Self, UnboundedReceiver<Action>), KeymapError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let keymap = config.keymap()?;
        let release_keys = config
            .plugins
            .pane_release_keys
            .iter()
            .filter_map(|k| k.parse().ok())
            .collect();
        let mut graph = GraphPane::new(clock.clone());
        graph.max_lanes = config.graph.max_lanes;
        graph.dates = config.graph.dates;
        let app = Self {
            ctx: Ctx::new(tx),
            graph_limit: config.graph.max_commits.max(1),
            config,
            keymap,
            theme: Theme::default(),
            status: StatusPane::new(repo.name()),
            repo,
            focus: PaneId::Graph,
            zoom: Zoom::Off,
            details_open: false,
            branches: BranchesPane::new(clock.clone()),
            graph,
            details: DetailsPane::default(),
            head: None,
            clock,
            modal: None,
            busy: Vec::new(),
            ticks: 0,
            notice_drawn: (None, 0),
            select_head: false,
            undo_pending: false,
            update_checking: false,
            worktree_count: (false, false),
            worktree_sizing: 0,
            worktree_size_queue: Vec::new(),
            worktree_sized: Vec::new(),
            reopen: None,
            watcher: None,
            plugins: None,
            panes: Vec::new(),
            next_pane_id: 1,
            release_keys,
            layout_rects: (Rect::new(0, 0, 120, 40), Rect::new(0, 0, 80, 15)),
            show_help: false,
            help_query: String::new(),
            error: None,
            notice: None,
            clipboard: None,
            editor: None,
            should_quit: false,
            pane_areas: Vec::new(),
            last_click: None,
            dirty: true,
            cursor_shown: false,
            details_wanted: None,
            details_jobs: Default::default(),
            file_diff_job: LatestJob::default(),
            history_job: LatestJob::default(),
            back: Vec::new(),
            refs_reload: Reload::default(),
            status_reload: Reload::default(),
        };
        Ok((app, rx))
    }

    /// Kicks off the initial loads and the file watcher.
    pub fn start(&mut self) {
        self.update(Action::Refresh);
        if self.config.watch && self.watcher.is_none() {
            match crate::watcher::watch(self.repo.clone(), self.ctx.clone()) {
                Ok(w) => self.watcher = Some(w),
                Err(e) => tracing::warn!("file watching disabled: {e}"),
            }
        }
        self.start_plugins();
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// The worktree asked for with [`Action::OpenWorktree`]: once the app
    /// has quit, the caller opens it in a new app.
    pub fn reopen(&self) -> Option<&Repo> {
        self.reopen.as_ref()
    }

    /// Takes [`reopen`](Self::reopen).
    pub fn take_reopen(&mut self) -> Option<Repo> {
        self.reopen.take()
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    pub fn zoom(&self) -> Zoom {
        self.zoom
    }

    /// Whether `pane` is on screen with the current layout, zoom and details state.
    pub fn shows(&self, pane: PaneId) -> bool {
        match self.zoom {
            Zoom::Full(full) => pane == full,
            _ => self.config.layout.shows(pane, self.view()),
        }
    }

    fn view(&self) -> View {
        View {
            details: self.details_open,
            wide: self.zoom == Zoom::Wide,
        }
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    pub fn graph(&self) -> &GraphPane {
        &self.graph
    }

    pub fn details(&self) -> &DetailsPane {
        &self.details
    }

    pub fn status(&self) -> &StatusPane {
        &self.status
    }

    pub fn branches(&self) -> &BranchesPane {
        &self.branches
    }

    /// The key binding context in effect: the focused pane's.
    pub fn keymap_context(&self) -> Context {
        self.focused().context()
    }

    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    pub fn jobs(&self) -> &JobTracker {
        self.ctx.jobs()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Whether a new frame could differ from the last one, clearing the flag.
    /// Input and actions that may change what's shown set it; the runner
    /// skips drawing while it's clear.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Takes the text a command copied, for the runner to send to the terminal.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// Takes the editor a command asked for, for the runner to start. It
    /// reports back with [`Action::EditorFinished`].
    pub fn take_editor(&mut self) -> Option<EditorLaunch> {
        self.editor.take()
    }

    /// The terminal's title: `<repo> - tt-git`, like lazygit's.
    pub fn terminal_title(&self) -> String {
        format!("{} - tt-git", self.repo.name())
    }

    /// Where each pane was drawn in the last frame.
    pub fn pane_area(&self, pane: PaneId) -> Option<Rect> {
        self.pane_areas.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)
    }

    /// Handles a terminal event.
    pub fn handle_event(&mut self, event: Event) {
        if let Event::Key(key) = &event {
            tracing::debug!(code = ?key.code, modifiers = ?key.modifiers, kind = ?key.kind, "key");
        }
        match event {
            // Windows reports releases too; only act on presses (and repeats).
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.dirty = true;
                self.handle_key(key);
            }
            Event::Mouse(m) if self.config.mouse => {
                // Only these do anything; any-motion capture also reports
                // every move.
                if matches!(
                    m.kind,
                    MouseEventKind::Down(MouseButton::Left) | MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                ) {
                    self.dirty = true;
                }
                self.handle_mouse(m);
            }
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    /// Whether the app wants terminal mouse capture.
    pub fn wants_mouse(&self) -> bool {
        self.config.mouse
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) {
        self.mouse(m);
        self.flush_details();
    }

    fn mouse(&mut self, m: MouseEvent) {
        if self.error.is_none()
            && let Some(Modal::Filter(panel)) = &mut self.modal
        {
            let outcome = panel.handle_mouse(m, self.layout_rects.0);
            self.modal_outcome(outcome);
            return;
        }
        // Popups and floating plugin panes own the screen.
        if self.modal.is_some() || self.error.is_some() || self.show_help || self.panes.iter().any(|p| p.is_modal()) {
            return;
        }
        let pos = Position::new(m.column, m.row);
        let Some((pane, area)) = self.pane_areas.iter().copied().find(|(_, r)| r.contains(pos)) else {
            return;
        };
        if pane == PaneId::Details && self.has_split_pane() {
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.focus = PaneId::Details;
            }
            return;
        }
        let body_row =
            (m.row > area.y && m.row < area.bottom().saturating_sub(1)).then(|| (m.row - area.y - 1) as usize);
        let ctx = self.ctx.clone();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if pane == PaneId::Details && m.modifiers.contains(KeyModifiers::CONTROL) {
                    if let Some(row) = body_row {
                        self.ctrl_click_details(row);
                    }
                    return;
                }
                let before = self.focus;
                self.focus = pane;
                let target = self.pane_mut(pane);
                let handled = target.on_click_at(pos, &ctx);
                let row = body_row.filter(|_| !handled);
                if let Some(row) = row {
                    target.on_click(row, &ctx);
                }
                if self.focus != before {
                    self.refresh_details(false);
                }
                let double = match row {
                    Some(row) => self.is_double_click(pane, row),
                    None => {
                        self.last_click = None;
                        false
                    }
                };
                if double && pane == PaneId::Graph && row.is_some_and(|r| self.graph.has_commit_at_row(r)) {
                    // As enter does: the commit fullscreen, files beside the diff.
                    self.update(Action::Command(Command::ExpandDetails));
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                let cmd = if down { Command::MoveDown } else { Command::MoveUp };
                for _ in 0..3 {
                    let target = self.pane_mut(pane);
                    if !target.on_scroll_at(pos, down, &ctx) {
                        target.on_command(cmd, &ctx);
                    }
                }
            }
            _ => {}
        }
    }

    /// Notes a click on `pane`'s body `row`, returning whether it's the
    /// second of a double click: the same row clicked again soon after.
    /// A third click starts over.
    fn is_double_click(&mut self, pane: PaneId, row: usize) -> bool {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(at, p, r)| p == pane && r == row && now.duration_since(at) <= DOUBLE_CLICK);
        self.last_click = (!double).then_some((now, pane, row));
        double
    }

    fn pane_mut(&mut self, pane: PaneId) -> &mut dyn Component {
        match pane {
            PaneId::Status => &mut self.status,
            PaneId::Branches => &mut self.branches,
            PaneId::Graph => &mut self.graph,
            PaneId::Details => &mut self.details,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.key(key);
        self.flush_details();
    }

    fn key(&mut self, key: KeyEvent) {
        let combo = KeyCombo::from(key);
        if self.error.is_none() && self.modal.is_none() && self.pane_key(combo) {
            return;
        }
        if self.error.is_some() {
            if matches!(combo.code, KeyCode::Esc | KeyCode::Enter) || combo == KeyCombo::char('q') {
                self.error = None;
            }
            return;
        }
        if let Some(modal) = &mut self.modal {
            let outcome = modal.handle_key(combo);
            self.modal_outcome(outcome);
            return;
        }
        if self.show_help {
            self.help_key(combo);
            return;
        }
        if self.focused().wants_text_input() {
            let ctx = self.ctx.clone();
            self.focused_mut().on_text_key(combo, &ctx);
            return;
        }
        let ctx = self.ctx.clone();
        if self.focused_mut().on_key(combo, &ctx) {
            return;
        }
        let context = self.focused().context();
        if let Some(custom) = self.keymap.lookup_custom(context, combo) {
            let action = custom.action.clone();
            let _ = self.invoke_plugin_action(&action, "keybinding");
            return;
        }
        if let Some(cmd) = self.keymap.lookup(context, combo) {
            self.update(Action::Command(cmd));
        }
    }

    /// Acts on what the open modal made of a key or click.
    fn modal_outcome(&mut self, outcome: ModalOutcome) {
        match outcome {
            ModalOutcome::Open => {}
            ModalOutcome::Cancelled => self.modal = None,
            ModalOutcome::Run(op) => {
                self.modal = None;
                self.update(Action::Run(op));
            }
            ModalOutcome::Command(cmd) => {
                self.modal = None;
                self.run_command(cmd);
            }
            ModalOutcome::Filter(filter) => {
                self.modal = None;
                self.set_graph_filter(filter);
            }
            ModalOutcome::GoToLine(line) => {
                self.modal = None;
                let ctx = self.ctx.clone();
                if let Some(view) = self.details.blame_mut() {
                    view.go_to_line(line, &ctx);
                }
            }
            ModalOutcome::Agent(req) => {
                self.modal = None;
                self.start_agent(req);
            }
            ModalOutcome::PluginAction(action) => {
                self.modal = None;
                let _ = self.invoke_plugin_action(&action, "palette");
            }
            ModalOutcome::PluginPane(plugin, entry) => {
                self.modal = None;
                let req = PaneRequest {
                    focus: true,
                    ..Default::default()
                };
                if let Err(e) = self.open_pane(&plugin, &entry, req) {
                    self.error = Some(e);
                }
            }
        }
    }

    /// Typing narrows the help popup's list. The help key closes it while
    /// nothing is typed; other bound keys only count for quit.
    fn help_key(&mut self, combo: KeyCombo) {
        let ctrl_or_alt = combo.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let bound = self.keymap.lookup(Context::Global, combo);
        match combo.code {
            KeyCode::Esc => self.show_help = false,
            KeyCode::Backspace => {
                self.help_query.pop();
            }
            KeyCode::Char('u') if combo.modifiers.contains(KeyModifiers::CONTROL) => self.help_query.clear(),
            KeyCode::Char(_) if bound == Some(Command::Help) && self.help_query.is_empty() => self.show_help = false,
            KeyCode::Char(c) if !ctrl_or_alt => self.help_query.push(c),
            _ if bound == Some(Command::Quit) => self.should_quit = true,
            _ => {}
        }
    }

    /// The help popup's rows, narrowed to those matching the typed query.
    pub fn help_rows(&self) -> Vec<(String, &'static str)> {
        let q = &self.help_query;
        let mut rows = self.all_help_rows();
        rows.retain(|(keys, label)| matches_filter(label, q) || matches_filter(keys, q));
        rows
    }

    /// (keys, label) for the focused pane's bound commands, then the global ones.
    fn all_help_rows(&self) -> Vec<(String, &'static str)> {
        let context = self.focused().context();
        let sections: [(Context, &[Command]); 2] =
            [(context, self.focused().commands()), (Context::Global, ALL_GLOBAL)];
        let mut rows = Vec::new();
        for (ctx, cmds) in sections {
            for cmd in cmds {
                let keys = self.keymap.keys_text(ctx, *cmd);
                if !keys.is_empty() {
                    rows.push((keys.to_string(), cmd.label()));
                }
            }
        }
        rows
    }

    /// How many rows [`all_help_rows`](Self::all_help_rows) has.
    fn all_help_row_count(&self) -> usize {
        let context = self.focused().context();
        let sections: [(Context, &[Command]); 2] =
            [(context, self.focused().commands()), (Context::Global, ALL_GLOBAL)];
        sections
            .iter()
            .map(|(ctx, cmds)| {
                cmds.iter()
                    .filter(|cmd| !self.keymap.keys_text(*ctx, **cmd).is_empty())
                    .count()
            })
            .sum()
    }

    pub fn update(&mut self, action: Action) {
        self.apply(action);
        self.flush_details();
    }

    fn apply(&mut self, action: Action) {
        // A tick only matters if plugins reloaded, the last frame showed the
        // cursor, a relative date drawn ("now", "5m") would read
        // differently, or the help bar is turning a spinner or fading a
        // notice. Word from a pane already closed (its process dies after)
        // changes nothing; anything else may change what's shown.
        let quiet = match &action {
            Action::Tick => true,
            Action::PaneOutput(id) | Action::PaneExited { id, .. } => !self.panes.iter().any(|p| p.id == *id),
            _ => false,
        };
        if !quiet {
            self.dirty = true;
        }
        match action {
            Action::Command(cmd) => self.run_command(cmd),
            Action::Tick => {
                self.ticks += 1;
                if self.poll_plugins() || self.cursor_shown || self.dates_stale() || self.help_bar_moving() {
                    self.dirty = true;
                }
            }
            Action::Socket(call) => self.handle_call(call),
            Action::PluginFinished(log) => self.plugin_finished(log),
            Action::PaneOutput(id) => self.pane_output(id),
            Action::PaneExited { id, status } => self.pane_exited(id, status),
            Action::Notify(msg) => self.notice = Some(msg),
            Action::Error(msg) => self.error = Some(msg),
            Action::Copy(text) => {
                self.notice = Some(format!("copied {text}"));
                self.clipboard = Some(text);
            }
            Action::Refresh => {
                self.load_refs();
                self.load_status();
                self.load_graph();
            }
            Action::OpenInEditor(target) => self.open_in_editor(target),
            Action::EditSnapshot(Ok(snap)) => {
                self.notice = Some(format!(
                    "{} isn't in the working tree: opened a read-only copy from {}",
                    snap.path, snap.commit
                ));
                self.start_editor(&snap.file, snap.line);
            }
            Action::EditSnapshot(Err(e)) => self.error = Some(e),
            Action::AgentStarted(result) => self.agent_started(result),
            Action::EditorFinished(result) => {
                match result {
                    Ok(Some(code)) if code != 0 => self.notice = Some(format!("editor exited with status {code}")),
                    Ok(_) => {}
                    Err(e) => self.error = Some(e),
                }
                // Pick up the edits even when the watcher is off.
                self.apply(Action::Refresh);
            }
            Action::GraphBatch {
                generation,
                commits,
                done,
            } => self.graph.apply_batch(generation, commits, done, &self.ctx),
            Action::BranchHistoryLoaded { generation, history } => {
                self.graph.apply_history(generation, history, &self.ctx);
                self.history_finished(generation);
            }
            Action::GraphFailed { generation, error } => {
                self.graph.load_failed(generation);
                self.error = Some(error);
                self.history_finished(generation);
            }
            Action::GraphTipsChecked { generation, tips } => {
                // A newer load started after the change, so it shows it.
                if generation == self.graph.generation() {
                    if tips.is_some_and(|t| self.graph.same_tips(&t)) {
                        self.graph.reload_unchanged(&self.ctx);
                    } else {
                        self.load_graph();
                    }
                }
            }
            Action::ReloadGraph => self.load_graph(),
            Action::GraphRestarted { generation } => self.graph.restart(generation),
            Action::LoadMoreCommits => {
                self.graph_limit = self.graph_limit.saturating_add(self.config.graph.max_commits.max(1));
                self.load_more_commits();
            }
            Action::RefsLoaded(Ok(snapshot)) => {
                if std::mem::take(&mut self.select_head)
                    && let Some(oid) = snapshot.head.oid()
                {
                    self.graph.select_when_loaded(oid.to_string(), &self.ctx);
                }
                self.graph.set_refs(&snapshot);
                let RefsSnapshot {
                    head,
                    refs,
                    branches,
                    base,
                    stashes,
                    worktrees,
                    ..
                } = *snapshot;
                self.branches.set_entries(branches, base, &refs, stashes, worktrees);
                if self.branches.tab() == BranchTab::Worktrees {
                    self.count_worktree_changes();
                    self.measure_worktree_sizes(false);
                }
                self.head = Some(head);
                self.track_head();
                // Follow the focused branch if the list changed under it.
                if self.graph.focus.is_some() {
                    self.focus_branch(self.branches.focus());
                }
            }
            Action::BranchSelected(oid) => self.graph.select_when_loaded(oid, &self.ctx),
            Action::CountWorktreeChanges => {
                self.count_worktree_changes();
                self.measure_worktree_sizes(true);
            }
            Action::WorktreeSize(path, size) => {
                self.branches.set_worktree_size(path, size);
                self.worktree_sizing = self.worktree_sizing.saturating_sub(1);
                self.start_size_jobs();
                if self.worktree_sizing == 0 {
                    self.measure_worktree_sizes(false);
                }
            }
            Action::WorktreeChanges(counts) => {
                self.branches.set_worktree_changes(counts);
                let (_, again) = std::mem::take(&mut self.worktree_count);
                if again {
                    self.count_worktree_changes();
                }
            }
            Action::OpenWorktree(path) => self.open_worktree(path),
            Action::StashSelected(stash) => {
                if self.focus == PaneId::Branches {
                    self.show_stash(stash);
                }
            }
            Action::BranchFocused(focus) => self.focus_branch(focus),
            Action::OpenModal(modal) => self.modal = Some(modal),
            Action::Run(op) => self.run_op(op),
            Action::Checkout(plan) => self.checkout(plan),
            Action::UndoPlanned(planned) => self.undo_planned(planned),
            Action::UpdateChecked(latest) => self.update_checked(latest),
            Action::OpDone { op, result, head } => self.op_done(op, result, head),
            Action::StatusLoaded(Ok(status)) => {
                self.status.set_status(status);
                self.refresh_staging();
                if self.focus == PaneId::Status {
                    self.refresh_details(true);
                }
            }
            Action::FileSelected(entry) => self.show_file(entry, false),
            Action::RepoChanged { refs, status } => {
                if refs {
                    self.load_refs();
                    self.check_graph();
                }
                if status {
                    self.load_status();
                }
            }
            Action::RefsLoaded(Err(e)) | Action::StatusLoaded(Err(e)) => self.error = Some(e),
            Action::Reloaded { generation, result } => {
                let refs = matches!(*result, Action::RefsLoaded(_));
                let reload = if refs {
                    &mut self.refs_reload
                } else {
                    &mut self.status_reload
                };
                let (apply, next) = reload.finished(generation);
                if apply {
                    self.apply(*result);
                }
                if let Some(generation) = next {
                    if refs {
                        self.spawn_refs(generation);
                    } else {
                        self.spawn_status(generation);
                    }
                }
            }
            Action::CommitSelected(oid) => {
                if matches!(self.focus, PaneId::Graph | PaneId::Branches) {
                    self.show_commit(oid);
                }
            }
            Action::DetailsLoaded { subject, text, syntax } => {
                self.details.set_content(&subject, &text, syntax);
                match subject {
                    DetailsSubject::Commit(oid) => {
                        self.details_finished(0, |l| matches!(l, DetailsLoad::Commit(o) if *o == oid))
                    }
                    DetailsSubject::File(path) => {
                        self.details_finished(2, |l| matches!(l, DetailsLoad::File(e) if e.path == path))
                    }
                    DetailsSubject::Stash(oid) => {
                        self.details_finished(4, |l| matches!(l, DetailsLoad::Stash(s) if s.oid == oid))
                    }
                    _ => {}
                }
            }
            Action::CommitFilesLoaded { oid, header, files } => {
                match files {
                    Ok(files) => self.details.set_commit_files(&oid, &header, files, &self.ctx),
                    Err(e) => self.details.set_content(
                        &DetailsSubject::CommitFiles(oid.clone()),
                        &format!("{header}\n{e}"),
                        Vec::new(),
                    ),
                }
                self.details_finished(1, |l| matches!(l, DetailsLoad::CommitFiles(o) if *o == oid));
            }
            Action::CommitFileSelected { oid, file } => {
                if let Some(item) = self.file_diff_job.request((oid, file), false) {
                    self.spawn_file_diff(item);
                }
            }
            Action::CommitFileDiffLoaded {
                oid,
                path,
                text,
                syntax,
            } => {
                self.details.set_file_diff(&oid, &path, &text, syntax);
                if let Some(next) = self.file_diff_job.finished(|(o, f)| *o == oid && f.path == path) {
                    self.spawn_file_diff(next);
                }
            }
            Action::StageDiffsLoaded {
                entry,
                unstaged,
                staged,
                syntax,
            } => {
                let path = entry.path.clone();
                self.details.set_stage_diffs(entry, unstaged, staged, syntax);
                self.details_finished(3, |l| matches!(l, DetailsLoad::Stage(e) if e.path == path));
            }
            Action::LoadBlame { id, rev, path } => {
                let repo = self.repo.clone();
                let highlight = self.config.syntax;
                self.ctx
                    .spawn(move || loaders::load_blame(repo, id, rev, path, highlight));
            }
            Action::BlameLoaded {
                id,
                result,
                syntax,
                changed,
                recency,
            } => {
                let ctx = self.ctx.clone();
                if let Some(view) = self.details.blame_mut() {
                    view.set_blame(id, result, syntax, changed, recency, &ctx);
                }
            }
            Action::StepBlame { id, rev, path, newest } => {
                let repo = self.repo.clone();
                self.ctx.spawn(move || loaders::step_blame(repo, id, rev, path, newest));
            }
            Action::BlameStepped { id, version } => {
                let ctx = self.ctx.clone();
                if let Some(view) = self.details.blame_mut() {
                    view.set_version(id, version, &ctx);
                }
            }
            Action::LoadCommitMessage(oid) => {
                let repo = self.repo.clone();
                self.ctx.spawn(move || loaders::load_commit_message(repo, oid));
            }
            Action::CommitMessageLoaded { oid, body } => {
                if let Some(view) = self.details.blame_mut() {
                    view.set_message(&oid, body);
                }
            }
            Action::FilterChoicesLoaded { tab, choices } => {
                if let Some(Modal::Filter(panel)) = &mut self.modal {
                    panel.set_choices(tab, choices);
                }
            }
        }
    }

    /// Reloads refs, after the load running now if there is one.
    fn load_refs(&mut self) {
        if let Some(generation) = self.refs_reload.request() {
            self.spawn_refs(generation);
        }
    }

    fn spawn_refs(&mut self, generation: u64) {
        let repo = self.repo.clone();
        let config = self.config.branches.clone();
        let done = self.ctx.spawn_watched(move || Action::Reloaded {
            generation,
            result: Box::new(loaders::load_refs(repo, config)),
        });
        self.refs_reload.started(generation, done);
    }

    /// Reloads the status, after the load running now if there is one.
    fn load_status(&mut self) {
        if let Some(generation) = self.status_reload.request() {
            self.spawn_status(generation);
        }
    }

    fn spawn_status(&mut self, generation: u64) {
        let repo = self.repo.clone();
        let done = self.ctx.spawn_watched(move || Action::Reloaded {
            generation,
            result: Box::new(loaders::load_status(repo)),
        });
        self.status_reload.started(generation, done);
    }

    /// Whether a relative date on screen would read differently now.
    fn dates_stale(&self) -> bool {
        let now = self.clock.now();
        (self.pane_area(PaneId::Graph).is_some() && self.graph.dates_stale(now))
            || (self.pane_area(PaneId::Branches).is_some() && self.branches.dates_stale(now))
    }

    fn load_graph(&mut self) {
        let load = self.graph.begin_load();
        self.graph.limit = self.graph_limit;
        let limit = self.graph_limit;
        if let Some(focus) = self.graph.focus.clone() {
            // Histories don't stream, so can't stop early: run one at a
            // time, skipping those replaced while waiting.
            let history = HistoryLoad { load, focus, limit };
            if let Some(history) = self.history_job.request(history, false) {
                self.spawn_history(history);
            }
            return;
        }
        let repo = self.repo.clone();
        let scope = self.graph.scope;
        let filter = self.graph.filter.clone();
        self.ctx
            .spawn_streaming(move |ctx| loaders::load_graph(repo, scope, None, filter, load, limit, None, ctx));
    }

    /// Loads up to the raised limit: only the commits after those shown
    /// when it can, otherwise all of them again.
    fn load_more_commits(&mut self) {
        let Some((load, skip, tips)) = self.graph.begin_more() else {
            self.load_graph();
            return;
        };
        self.graph.limit = self.graph_limit;
        let limit = self.graph_limit;
        let repo = self.repo.clone();
        let scope = self.graph.scope;
        let filter = self.graph.filter.clone();
        let from = Some(loaders::Continue { skip, tips });
        self.ctx
            .spawn_streaming(move |ctx| loaders::load_graph(repo, scope, None, filter, load, limit, from, ctx));
    }

    fn spawn_history(&mut self, history: HistoryLoad) {
        if !history.load.is_current() {
            return;
        }
        let repo = self.repo.clone();
        let scope = self.graph.scope;
        let filter = self.graph.filter.clone();
        let ctx = self.ctx.clone();
        let HistoryLoad { load, focus, limit } = history.clone();
        let done = self
            .ctx
            .spawn_watched(move || loaders::load_graph(repo, scope, Some(focus), filter, load, limit, None, &ctx));
        self.history_job.started(history, done);
    }

    /// Graph load `generation` ended; starts the history waiting on it.
    fn history_finished(&mut self, generation: u64) {
        if let Some(next) = self.history_job.finished(|h| h.load.generation() == generation) {
            self.spawn_history(next);
        }
    }

    /// After refs changed: reloads the graph unless what it walks from is
    /// the same as for its current load. Focused histories and loads that
    /// haven't noted that (yet) reload as always.
    fn check_graph(&mut self) {
        if self.graph.focus.is_some() || !self.graph.has_tips() {
            self.load_graph();
            return;
        }
        let repo = self.repo.clone();
        let scope = self.graph.scope;
        let generation = self.graph.generation();
        self.ctx.spawn(move || loaders::graph_tips(repo, scope, generation));
    }

    /// Opens the graph's filter panel and loads the authors and files it lists.
    fn open_graph_filter(&mut self) {
        self.modal = Some(Modal::Filter(Box::new(FilterPanel::new(self.graph.filter.clone()))));
        let repo = self.repo.clone();
        self.ctx.spawn(move || loaders::load_filter_authors(repo));
        let repo = self.repo.clone();
        self.ctx.spawn(move || loaders::load_filter_files(repo));
    }

    /// Narrows the graph to `filter`'s commits (leaving a focused branch,
    /// which the filter doesn't apply to), or shows them all again.
    fn set_graph_filter(&mut self, filter: LogFilter) {
        if self.graph.filter == filter && self.graph.focus.is_none() {
            return;
        }
        self.graph.filter = filter;
        self.graph.focus = None;
        self.load_graph();
    }

    /// Shows only `focus`'s history in the graph (with its tip selected), or
    /// the whole graph again.
    fn focus_branch(&mut self, focus: Option<BranchFocus>) {
        if self.graph.focus == focus {
            return;
        }
        let tip = focus.as_ref().map(|f| f.tip.clone());
        self.graph.focus = focus;
        self.load_graph();
        if let Some(tip) = tip {
            self.graph.select_when_loaded(tip, &self.ctx);
        }
    }

    fn run_op(&mut self, op: GitOp) {
        let label = op.label();
        let (doing, done) = self.describe_op(&op);
        // A second pull, push or fetch would only race the first.
        if matches!(op, GitOp::Push | GitOp::Pull | GitOp::FetchAll | GitOp::PushTag { .. })
            && self.busy.iter().any(|b| b.label == label)
        {
            let verb = doing.split(' ').next().unwrap_or_default();
            self.renew_notice(format!("already {verb}"));
            return;
        }
        if matches!(op, GitOp::ApplyPatch { .. })
            && let Some(view) = self.details.stage_mut()
        {
            view.mark_applying();
        }
        if matches!(op, GitOp::Undo { .. }) {
            self.undo_pending = true;
        }
        // The help bar shows it running from here on.
        self.notice = None;
        self.busy.push(Busy {
            label,
            doing,
            done,
            since: self.ticks,
        });
        if let GitOp::SelfUpdate { tag } = op {
            self.ctx.spawn_streaming(move |ctx| loaders::self_update(ctx, tag));
            return;
        }
        let repo = self.repo.clone();
        self.ctx.spawn(move || loaders::run_op(repo, op));
    }

    /// What the help bar says while `op` runs and once it's done. Pulls,
    /// pushes and fetches say where they go; anything else is its label.
    fn describe_op(&self, op: &GitOp) -> (String, String) {
        let branch = self.status.status().map(|s| &s.branch);
        let head = branch.and_then(|b| b.head.as_deref());
        let upstream = branch.and_then(|b| b.upstream.as_deref());
        let both = |doing: &str, done: &str, what: String| {
            if what.is_empty() {
                (doing.to_string(), done.to_string())
            } else {
                (format!("{doing} {what}"), format!("{done} {what}"))
            }
        };
        match op {
            GitOp::Push => {
                let what = match (head, upstream) {
                    (Some(head), Some(up)) => format!("{head} → {up}"),
                    (Some(head), None) => head.to_string(),
                    _ => String::new(),
                };
                both("pushing", "pushed", what)
            }
            GitOp::Pull => {
                let what = match (head, upstream) {
                    (Some(head), Some(up)) => format!("{up} into {head}"),
                    _ => String::new(),
                };
                both("pulling", "pulled", what)
            }
            GitOp::FetchAll => both("fetching", "fetched", "all remotes".into()),
            GitOp::PushTag { name } => both("pushing", "pushed", format!("tag {name}")),
            _ => (op.label(), op.label()),
        }
    }

    /// The running operations as the help bar shows them: the spinner, what
    /// one is doing (just the verbs of several), and for how long once it's
    /// been a second.
    fn busy_line(&self) -> Option<String> {
        let first = self.busy.first()?;
        let doing = if self.busy.len() == 1 {
            first.doing.clone()
        } else {
            let verbs: Vec<_> = self
                .busy
                .iter()
                .map(|b| b.doing.split(' ').next().unwrap_or_default())
                .collect();
            verbs.join(" · ")
        };
        let spinner = SPINNER[(self.ticks % SPINNER.len() as u64) as usize];
        let secs = (self.ticks - first.since) / TICKS_PER_SEC;
        Some(if secs >= 1 {
            format!("{spinner} {doing}  {secs}s")
        } else {
            format!("{spinner} {doing}")
        })
    }

    /// Shows `notice` as new even if the same text is showing already, so a
    /// second `✓ pushed …` turns green again.
    fn renew_notice(&mut self, notice: String) {
        self.notice = Some(notice);
        self.notice_drawn.0 = None;
    }

    /// Ticks since the notice on screen was first drawn.
    fn notice_age(&self) -> u64 {
        self.ticks - self.notice_drawn.1
    }

    /// The help bar changes from one tick to the next: a spinner is
    /// turning, or a fresh `✓` notice has yet to fade.
    fn help_bar_moving(&self) -> bool {
        !self.busy.is_empty()
            || self.notice.as_deref().is_some_and(|n| n.starts_with('✓')) && self.notice_age() <= SUCCESS_FADE
    }

    /// Checks out `plan`, or with uncommitted changes asks to stash them first.
    fn checkout(&mut self, plan: CheckoutPlan) {
        let changed = self.status.entries().len();
        if changed == 0 {
            self.run_op(GitOp::Checkout { plan, stash: false });
            return;
        }
        let target = match &plan {
            CheckoutPlan::Local(name) | CheckoutPlan::Track { name, .. } => name.clone(),
            CheckoutPlan::Detach(oid) => format!("{} (detached HEAD)", &oid[..oid.len().min(7)]),
        };
        let files = if changed == 1 {
            "1 file has".to_string()
        } else {
            format!("{changed} files have")
        };
        self.modal = Some(Modal::confirm(
            "Uncommitted changes",
            format!(
                "{files} uncommitted changes.\nStash them and check out {target}?\n\n\
                 They stay on the branches pane's Stash tab to pop back later."
            ),
            GitOp::Checkout { plan, stash: true },
        ));
    }

    /// Counts each worktree's changed files in the background, after the
    /// count running now if there is one.
    fn count_worktree_changes(&mut self) {
        if self.worktree_count.0 {
            self.worktree_count.1 = true;
            return;
        }
        self.worktree_count = (true, false);
        let repo = self.repo.clone();
        let worktrees = self.branches.countable_worktree_heads();
        let base = self.branches.base().map(|b| b.base_ref.clone());
        self.ctx
            .spawn(move || loaders::count_worktree_changes(repo, worktrees, base));
    }

    /// Measures each worktree's disk space in the background, one job per
    /// worktree so the quick ones show while a big one is still walked:
    /// `again` even if the same worktrees were measured before, else only
    /// when the list changed. One round at a time; when it's done the list
    /// is checked again.
    fn measure_worktree_sizes(&mut self, again: bool) {
        let paths = self.branches.countable_worktrees();
        if self.worktree_sizing > 0 || !self.worktree_size_queue.is_empty() || (!again && paths == self.worktree_sized)
        {
            return;
        }
        self.worktree_size_queue = paths
            .iter()
            .rev()
            .map(|path| {
                let nested = paths
                    .iter()
                    .filter(|p| *p != path && p.starts_with(path))
                    .cloned()
                    .collect();
                (path.clone(), nested)
            })
            .collect();
        self.worktree_sized = paths;
        self.start_size_jobs();
    }

    /// Starts measuring queued worktrees, at most [`SIZE_JOBS`] at a time.
    fn start_size_jobs(&mut self) {
        while self.worktree_sizing < SIZE_JOBS
            && let Some((path, nested)) = self.worktree_size_queue.pop()
        {
            self.worktree_sizing += 1;
            self.ctx.spawn(move || loaders::measure_worktree_size(path, nested));
        }
    }

    /// Quits so the worktree at `path` can be opened in its place, unless a
    /// git write is still running here or it isn't a repository.
    fn open_worktree(&mut self, path: std::path::PathBuf) {
        if let Some(op) = self.busy.first() {
            self.notice = Some(format!("wait for {} to finish first", op.label));
            return;
        }
        match Repo::discover(&path, self.repo.git()) {
            Ok(repo) => {
                tracing::info!("opening worktree {}", repo.root().display());
                self.notice = Some(format!("opening {}…", repo.name()));
                self.reopen = Some(repo);
                self.should_quit = true;
            }
            Err(e) => self.error = Some(format!("can't open {}:\n{e}", path.display())),
        }
    }

    /// Works out the next undo step in the background.
    fn plan_undo(&mut self) {
        if self.undo_pending {
            return;
        }
        self.undo_pending = true;
        let repo = self.repo.clone();
        self.ctx.spawn(move || loaders::plan_undo(repo));
    }

    /// Takes a planned undo step, asking first if there are
    /// uncommitted changes it would otherwise put at risk.
    fn undo_planned(&mut self, planned: Result<Result<UndoStep, NoStep>, String>) {
        self.undo_pending = false;
        let step = match planned {
            Ok(Ok(step)) => step,
            Ok(Err(none)) if none.is_notice() => {
                self.notice = Some(none.message());
                return;
            }
            Ok(Err(none)) => {
                self.notice = None;
                self.error = Some(none.message());
                return;
            }
            Err(err) => {
                self.notice = None;
                self.error = Some(format!("reading the reflog failed:\n{err}"));
                return;
            }
        };
        let changed = self.status.entries().len();
        // Undoing a commit only moves the branch, so there's
        // nothing to ask about.
        if changed == 0 || !step.target.touches_worktree() {
            self.run_op(GitOp::Undo { step, stash: false });
            return;
        }
        let files = if changed == 1 {
            "1 file has".to_string()
        } else {
            format!("{changed} files have")
        };
        let target = step.target.display();
        let message = if step.target.is_switch() {
            format!(
                "{files} uncommitted changes.\nStash them and {}, switching to {target}?\n\n\
                 They stay on the branches pane's Stash tab to pop back later.",
                step.label()
            )
        } else {
            format!(
                "{files} uncommitted changes.\nStash them, {} (reset to {target}), and put them back?\n\n\
                 If they don't go back cleanly, they stay on the branches pane's Stash tab.",
                step.label()
            )
        };
        self.modal = Some(Modal::confirm(
            "Uncommitted changes",
            message,
            GitOp::Undo { step, stash: true },
        ));
    }

    /// Looks up the latest tt-git release in the background, unless this
    /// binary can't be updated (a cargo build, say).
    fn check_update(&mut self) {
        if self.update_checking || self.busy.iter().any(|b| b.label.starts_with("update tt-git")) {
            return;
        }
        if let Err(e) = tt_git_core::update::current_install_dir() {
            self.error = Some(format!("can't update tt-git:\n{e}"));
            return;
        }
        self.update_checking = true;
        self.notice = Some("checking for a tt-git update…".into());
        self.ctx.spawn(loaders::check_update);
    }

    /// Offers to install the latest release when it's newer than this one.
    fn update_checked(&mut self, latest: Result<String, String>) {
        self.update_checking = false;
        let current = tt_git_core::update::CURRENT_VERSION;
        match latest {
            Ok(tag) if tt_git_core::update::is_newer(&tag, current) => {
                self.notice = None;
                self.modal = Some(Modal::confirm(
                    "Update tt-git",
                    format!(
                        "tt-git {tag} is out (you have {current}).\nDownload and install it now?\n\n\
                         Restart tt-git afterwards to use it."
                    ),
                    GitOp::SelfUpdate { tag },
                ));
            }
            Ok(_) => self.notice = Some(format!("tt-git {current} is up to date")),
            Err(e) => {
                self.notice = None;
                self.error = Some(format!("checking for a tt-git update failed:\n{e}"));
            }
        }
    }

    fn op_done(&mut self, op: GitOp, result: Result<String, String>, head: Option<String>) {
        let label = op.label();
        let done = match self.busy.iter().position(|b| b.label == label) {
            Some(pos) => self.busy.remove(pos).done,
            None => label.clone(),
        };
        // Nothing in the repository changed.
        if let (GitOp::SelfUpdate { tag }, Ok(_)) = (&op, &result) {
            self.notice = Some(format!("✓ installed tt-git {tag}; restart tt-git to use it"));
            return;
        }
        if matches!(op, GitOp::Undo { .. }) {
            self.undo_pending = false;
        }
        // The step was taken, but the stashed changes didn't go back.
        let kept =
            matches!(&result, Err(err) if matches!(op, GitOp::Undo { .. }) && err.starts_with(UNDO_CHANGES_KEPT));
        if (result.is_ok() || kept)
            && matches!(
                op,
                GitOp::Commit { .. }
                    | GitOp::Checkout { .. }
                    | GitOp::CreateBranch { worktree: false, .. }
                    | GitOp::Pull
                    | GitOp::Merge { .. }
                    | GitOp::Rebase { .. }
                    | GitOp::CherryPick { .. }
                    | GitOp::Continue(_)
                    | GitOp::Abort(_)
                    | GitOp::StashBranch { .. }
                    | GitOp::Undo { .. }
            )
        {
            self.select_head = true;
        }
        if result.is_ok() {
            self.op_event(&op, head.as_deref());
        }
        match result {
            Ok(_) => self.renew_notice(format!("✓ {done}")),
            Err(err) => match op {
                GitOp::DeleteBranch { name, force: false } if err.contains("not fully merged") => {
                    self.notice = None;
                    self.modal = Some(Modal::confirm(
                        "Force delete?",
                        format!("{name} is not fully merged. Delete it anyway (git branch -D)?"),
                        GitOp::DeleteBranch { name, force: true },
                    ));
                }
                GitOp::RemoveWorktree {
                    path,
                    name,
                    branch,
                    force: false,
                } if err.contains("modified or untracked") => {
                    self.notice = None;
                    self.modal = Some(Modal::confirm(
                        "Uncommitted changes",
                        format!(
                            "{name} has uncommitted or untracked changes.\n\
                             Remove it anyway and lose them for good (git worktree remove --force)?"
                        ),
                        GitOp::RemoveWorktree {
                            path,
                            name,
                            branch,
                            force: true,
                        },
                    ));
                }
                GitOp::Undo { .. } if kept => {
                    self.notice = None;
                    self.error = Some(err);
                }
                _ => {
                    self.notice = None;
                    self.error = Some(format!("{label} failed:\n{err}"));
                }
            },
        }
        self.update(Action::Refresh);
    }

    /// Points the details pane at whatever the focused pane has selected.
    fn refresh_details(&mut self, force: bool) {
        // Leaving details leaves what it would go back to.
        if self.focus != PaneId::Details {
            self.back.clear();
        }
        match self.focus {
            PaneId::Status => {
                if let Some(entry) = self.status.selected().cloned() {
                    self.show_file(entry, force);
                } else if force {
                    self.details.clear();
                }
            }
            PaneId::Graph | PaneId::Branches => {
                let stash = (self.focus == PaneId::Branches)
                    .then(|| self.branches.selected_stash().cloned())
                    .flatten();
                if let Some(stash) = stash {
                    self.show_stash(stash);
                } else if let Some(oid) = self.graph.selected().map(|c| c.oid.to_string()) {
                    self.show_commit(oid);
                }
            }
            PaneId::Details => {}
        }
    }

    fn show_file(&mut self, entry: StatusEntry, force: bool) {
        let subject = DetailsSubject::File(entry.path.clone());
        if !force && self.details.subject() == Some(&subject) {
            return;
        }
        self.details.request(subject, format!("Diff {}", entry.path));
        self.want_details(DetailsLoad::File(entry), force);
    }

    fn show_stash(&mut self, stash: Stash) {
        let subject = DetailsSubject::Stash(stash.oid.clone());
        if self.details.subject() == Some(&subject) {
            return;
        }
        self.details.request(subject, stash.name());
        self.want_details(DetailsLoad::Stash(stash), false);
    }

    /// Opens the selected stash fullscreen.
    fn expand_stash(&mut self) {
        let Some(stash) = self.branches.selected_stash().cloned() else {
            return;
        };
        self.show_stash(stash);
        self.details_open = true;
        self.zoom = Zoom::Full(PaneId::Details);
        self.focus = PaneId::Details;
    }

    /// Opens the selected file fullscreen to stage it by hunk or line.
    fn open_staging(&mut self) {
        let Some(entry) = self.status.selected().cloned() else {
            return;
        };
        self.details.request_stage(entry.clone());
        self.want_details(DetailsLoad::Stage(entry), true);
        self.details_open = true;
        self.zoom = Zoom::Full(PaneId::Details);
        self.focus = PaneId::Details;
    }

    /// The status pane's selected file in the editor, at its first change
    /// once that's loaded.
    fn edit_status_file(&mut self) {
        let Some(entry) = self.status.selected().cloned() else {
            self.notice = Some("no file to open here".into());
            return;
        };
        let repo = self.repo.clone();
        self.ctx.spawn(move || loaders::status_edit_target(repo, entry));
    }

    /// Opens `target` in the editor: the working-tree file, or if it isn't
    /// there, a read-only copy from the target's commit.
    fn open_in_editor(&mut self, target: EditTarget) {
        let file = editor::abs_path(self.repo.root(), &target.path);
        if file.exists() {
            self.start_editor(&file, target.line);
        } else if target.rev.is_some() {
            let repo = self.repo.clone();
            let dir = std::env::temp_dir().join("tt-git").join(self.repo.name());
            self.ctx.spawn(move || loaders::edit_snapshot(repo, dir, target));
        } else {
            self.notice = Some(format!("{} isn't in the working tree", target.path));
        }
    }

    /// Works out the editor command for `file` and leaves it for the
    /// runner to start.
    fn start_editor(&mut self, file: &std::path::Path, line: Option<u32>) {
        let repo = &self.repo;
        let cfg = &self.config.editor;
        let (command, from) = editor::choose(cfg, |k| std::env::var(k).ok(), || repo.core_editor());
        match editor::launch(cfg, &command, from, repo.root(), file, line) {
            Ok(launch) => self.editor = Some(launch),
            Err(e) => self.error = Some(e),
        }
    }

    /// Leaves the staging view for the status pane.
    fn close_staging(&mut self) {
        self.details.clear();
        self.details_open = false;
        if self.zoom == Zoom::Full(PaneId::Details) {
            self.zoom = Zoom::Off;
        }
        self.focus = PaneId::Status;
        self.refresh_details(true);
    }

    /// Reloads the staging view after the status changed, or goes back to
    /// the status pane once its file has no changes left.
    fn refresh_staging(&mut self) {
        let Some(path) = self.details.stage().map(|v| v.path().to_string()) else {
            return;
        };
        match self.status.entries().iter().find(|e| e.path == path).cloned() {
            Some(entry) => self.want_details(DetailsLoad::Stage(entry), true),
            None => self.close_staging(),
        }
    }

    /// Shows `oid` in details, staying in the files view if that's open.
    fn show_commit(&mut self, oid: String) {
        if matches!(self.details.subject(), Some(DetailsSubject::CommitFiles(_))) {
            self.show_commit_files(oid);
        } else {
            self.show_commit_plain(oid);
        }
    }

    fn show_commit_plain(&mut self, oid: String) {
        let subject = DetailsSubject::Commit(oid.clone());
        if self.details.subject() == Some(&subject) {
            return;
        }
        let short = oid.chars().take(7).collect::<String>();
        self.details.request(subject, format!("Commit {short}"));
        self.want_details(DetailsLoad::Commit(oid), false);
    }

    fn show_commit_files(&mut self, oid: String) {
        let subject = DetailsSubject::CommitFiles(oid.clone());
        if self.details.subject() == Some(&subject) {
            return;
        }
        let short = oid.chars().take(7).collect::<String>();
        self.details.request(subject, format!("Commit {short} files"));
        self.want_details(DetailsLoad::CommitFiles(oid), false);
    }

    /// Notes the load the details subject now needs. It starts at the end
    /// of the current input or action, once details is on screen.
    fn want_details(&mut self, load: DetailsLoad, force: bool) {
        let force = force || matches!(&self.details_wanted, Some((w, true)) if w.subject() == load.subject());
        self.details_wanted = Some((load, force));
    }

    /// Whether the details pane is drawn (a split plugin pane takes its place).
    fn details_visible(&self) -> bool {
        self.shows(PaneId::Details) && !self.has_split_pane()
    }

    /// Starts the wanted details load if details is on screen.
    fn flush_details(&mut self) {
        if self.details_wanted.is_none() || !self.details_visible() {
            return;
        }
        if let Some((load, force)) = self.details_wanted.take() {
            self.start_details(load, force);
        }
    }

    /// Starts `load` if it's still for the subject and details is on screen,
    /// one at a time per kind; otherwise keeps it until details shows.
    fn start_details(&mut self, load: DetailsLoad, force: bool) {
        if self.details.subject() != Some(&load.subject()) {
            return;
        }
        if !self.details_visible() {
            if self.details_wanted.is_none() {
                self.details_wanted = Some((load, true));
            }
            return;
        }
        let kind = load.kind();
        let Some(load) = self.details_jobs[kind].request(load, force) else {
            return;
        };
        let repo = self.repo.clone();
        let job = load.clone();
        let highlight = self.config.syntax;
        let done = self.ctx.spawn_watched(move || match job {
            DetailsLoad::Commit(oid) => loaders::load_commit_details(repo, oid, highlight),
            DetailsLoad::CommitFiles(oid) => loaders::load_commit_files(repo, oid),
            DetailsLoad::File(entry) => loaders::load_file_diff(repo, entry, highlight),
            DetailsLoad::Stage(entry) => loaders::load_stage_diffs(repo, entry, highlight),
            DetailsLoad::Stash(stash) => loaders::load_stash_details(repo, stash, highlight),
        });
        self.details_jobs[kind].started(load, done);
    }

    /// The running details load of `kind` reported back, if `is_it` says
    /// so; starts the one waiting on it.
    fn details_finished(&mut self, kind: usize, is_it: impl FnOnce(&DetailsLoad) -> bool) {
        if let Some(next) = self.details_jobs[kind].finished(is_it) {
            self.start_details(next, false);
        }
    }

    /// Loads a files-view diff, unless the view has moved on from it.
    fn spawn_file_diff(&mut self, item: (String, CommitFile)) {
        let (oid, file) = item.clone();
        let wanted = self.details.subject() == Some(&DetailsSubject::CommitFiles(oid.clone()))
            && self.details.selected_file().is_some_and(|f| f.path == file.path);
        if !wanted {
            return;
        }
        let repo = self.repo.clone();
        let highlight = self.config.syntax;
        let done = self
            .ctx
            .spawn_watched(move || loaders::load_commit_file_diff(repo, oid, file, highlight));
        self.file_diff_job.started(item, done);
    }

    /// Puts what details shows aside for `esc` to come back to.
    fn push_back(&mut self) {
        let label = if self.focus == PaneId::Details {
            self.details.back_label()
        } else {
            // The pane's name: `Status`.
            format!("{:?}", self.focus)
        };
        let details = self.details.save();
        self.back.push(Back {
            details,
            label,
            focus: self.focus,
            zoom: self.zoom,
            details_open: self.details_open,
        });
    }

    /// Goes back to the view the one shown was opened from, as it was.
    fn go_back(&mut self) {
        let Some(back) = self.back.pop() else { return };
        let ctx = self.ctx.clone();
        self.details.restore(back.details, &ctx);
        if let Some(view) = self.details.blame_mut() {
            view.set_dates(self.graph.dates);
        }
        self.focus = back.focus;
        self.zoom = back.zoom;
        self.details_open = back.details_open;
        // A load that finished while put aside was dropped: ask again.
        if self.details.loading() {
            match self.details.subject().cloned() {
                Some(DetailsSubject::Commit(oid)) => self.want_details(DetailsLoad::Commit(oid), true),
                Some(DetailsSubject::CommitFiles(oid)) => self.want_details(DetailsLoad::CommitFiles(oid), true),
                _ if self.focus != PaneId::Details => self.refresh_details(true),
                _ => {}
            }
        }
    }

    /// Opens a blame of `path` as of `rev` (the working tree when `None`)
    /// fullscreen, on `line`. `esc` comes back to what was showing.
    fn open_blame(&mut self, path: String, rev: Option<String>, label: String, line: u32) {
        self.push_back();
        let view = BlameView::open(path, rev, label, line, self.graph.dates, self.clock.clone(), &self.ctx);
        self.details.open_blame(view);
        self.details.set_trail(self.back.last().map(|b| b.label.clone()));
        self.details_open = true;
        self.zoom = Zoom::Full(PaneId::Details);
        self.focus = PaneId::Details;
    }

    /// Blames the status pane's selected file in the working tree.
    fn blame_status_file(&mut self) {
        let Some(entry) = self.status.selected().cloned() else {
            self.notice = Some("no file to blame here".into());
            return;
        };
        if entry.is_untracked() || entry.index == FileState::Added {
            self.notice = Some(format!("{} isn't committed yet: nothing to blame", entry.path));
            return;
        }
        // A file deleted from the working tree is blamed as it was in HEAD.
        let (rev, label) = if entry.worktree == FileState::Deleted || entry.index == FileState::Deleted {
            (Some("HEAD".to_string()), "HEAD")
        } else {
            (None, "working tree")
        };
        self.open_blame(entry.path, rev, label.to_string(), 1);
    }

    /// Blames the file details shows, as of the commit it's from (the
    /// working tree for a working-tree diff), on the line at the top.
    fn blame_details_file(&mut self) {
        if self.details.stage().is_some() || self.details.blame().is_some() {
            return;
        }
        match self.details.edit_target() {
            Ok(target) => {
                let label = target
                    .rev
                    .as_deref()
                    .map_or_else(|| "working tree".to_string(), short_rev);
                self.open_blame(target.path, target.rev, label, target.line.unwrap_or(1));
            }
            Err(why) => self.notice = Some(why),
        }
    }

    /// Opens the commit behind blame's selected line, with that file
    /// selected and the line marked in its diff. `esc` comes back.
    fn open_blame_commit(&mut self) {
        let Some((commit, path, line)) = self.details.blame().and_then(BlameView::selected) else {
            return;
        };
        if commit.is_uncommitted() {
            self.notice = Some("this line isn't committed yet: there's no commit to open".into());
            return;
        }
        let (oid, path) = (commit.oid.clone(), path.to_string());
        self.push_back();
        self.show_commit_files(oid);
        self.details.focus_file(path, line);
        self.details.set_trail(self.back.last().map(|b| b.label.clone()));
    }

    /// How many lines the focused blame has; 0 when blame isn't focused
    /// or hasn't loaded.
    fn blame_line_count(&self) -> usize {
        match self.details.blame() {
            Some(view) if self.focus == PaneId::Details && self.details.stage().is_none() => view.line_count(),
            _ => 0,
        }
    }

    /// `t` in blame: the same date style as the graph's, switched for both.
    fn toggle_blame_dates(&mut self) {
        self.graph.dates = self.graph.dates.toggled();
        let dates = self.graph.dates;
        if let Some(view) = self.details.blame_mut() {
            view.set_dates(dates);
        }
    }

    /// Opens the selected commit fullscreen as changed files beside a diff.
    fn expand_details(&mut self) {
        let Some(oid) = self.graph.selected().map(|c| c.oid.to_string()) else {
            return;
        };
        self.show_commit_files(oid);
        self.details_open = true;
        self.zoom = Zoom::Full(PaneId::Details);
        self.focus = PaneId::Details;
    }

    /// Switches details between the plain commit and its changed files
    /// beside or below the selected file's diff.
    fn toggle_files(&mut self) {
        match self.details.subject().cloned() {
            Some(DetailsSubject::Commit(oid)) => self.show_commit_files(oid),
            Some(DetailsSubject::CommitFiles(oid)) => self.show_commit_plain(oid),
            _ => {}
        }
    }

    fn run_command(&mut self, cmd: Command) {
        let before = self.focus;
        self.dispatch(cmd);
        if self.focus != before {
            self.settle_focus(before);
            self.refresh_details(false);
        }
    }

    fn step_zoom(&mut self) {
        self.zoom = match self.zoom {
            Zoom::Off => {
                if self.focus != PaneId::Details {
                    self.focus = PaneId::Graph;
                }
                Zoom::Full(self.focus)
            }
            Zoom::Wide | Zoom::Full(_) => Zoom::Off,
        };
    }

    /// Makes the newly focused pane visible: unzoom, then open details (to
    /// focus it) or close it (to focus a pane it covers). Falls back to
    /// `before` if neither helps.
    fn settle_focus(&mut self, before: PaneId) {
        if self.shows(self.focus) {
            return;
        }
        self.zoom = Zoom::Off;
        if self.shows(self.focus) {
            return;
        }
        self.details_open = self.focus == PaneId::Details;
        if !self.shows(self.focus) {
            self.focus = before;
        }
    }

    fn toggle_details(&mut self) {
        if self.shows(PaneId::Details) {
            self.close_details();
        } else {
            self.details_open = true;
            // A fullscreen graph has no room for details; drop back to wide.
            if self.zoom == Zoom::Full(PaneId::Graph) {
                self.zoom = Zoom::Wide;
            }
        }
    }

    fn close_details(&mut self) {
        // Leave the files view: next time details opens it's the plain commit.
        if let Some(DetailsSubject::CommitFiles(oid)) = self.details.subject().cloned() {
            self.show_commit_plain(oid);
        }
        self.details_open = false;
        if self.zoom == Zoom::Full(PaneId::Details) {
            self.zoom = Zoom::Off;
        }
        if self.focus == PaneId::Details {
            // A stash was opened from the branches pane; go back there.
            self.focus = if matches!(self.details.subject(), Some(DetailsSubject::Stash(_))) {
                PaneId::Branches
            } else {
                PaneId::Graph
            };
        }
    }

    fn dispatch(&mut self, cmd: Command) {
        match cmd {
            Command::Quit => self.should_quit = true,
            Command::Help => {
                self.show_help = !self.show_help;
                self.help_query.clear();
            }
            Command::FocusNext => self.cycle_focus(1),
            Command::FocusPrev => self.cycle_focus(PaneId::ALL.len() - 1),
            Command::FocusLeft | Command::FocusDown | Command::FocusUp | Command::FocusRight => self.focus_toward(cmd),
            Command::FocusStatus => self.focus = PaneId::Status,
            Command::FocusBranches => self.focus = PaneId::Branches,
            Command::FocusGraph => self.focus = PaneId::Graph,
            Command::FocusDetails => self.focus = PaneId::Details,
            Command::Zoom => self.step_zoom(),
            Command::ToggleDetails if self.focus == PaneId::Graph => self.toggle_details(),
            Command::ExpandDetails if self.focus == PaneId::Graph => self.expand_details(),
            Command::ExpandDetails if self.focus == PaneId::Branches => self.expand_stash(),
            // With details already closed, esc in the graph drops its filter.
            Command::CloseDetails
                if self.focus == PaneId::Graph && !self.shows(PaneId::Details) && self.graph.is_filtered() =>
            {
                self.set_graph_filter(LogFilter::default())
            }
            Command::OpenStaging if self.focus == PaneId::Status => self.open_staging(),
            // esc in a view opened on top of another goes back to it.
            Command::CloseDetails if self.focus == PaneId::Details && !self.back.is_empty() => self.go_back(),
            Command::Blame if self.focus == PaneId::Status => self.blame_status_file(),
            Command::Blame if self.focus == PaneId::Details => self.blame_details_file(),
            Command::OpenCommit if self.focus == PaneId::Details => self.open_blame_commit(),
            Command::ToggleDates if self.focus == PaneId::Details && self.details.blame().is_some() => {
                self.toggle_blame_dates()
            }
            Command::OpenInEditor if self.focus == PaneId::Status => self.edit_status_file(),
            Command::CloseDetails if self.focus == PaneId::Details && self.details.stage().is_some() => {
                self.close_staging()
            }
            // Commit straight from the staging view.
            Command::Commit if self.focus == PaneId::Details && self.details.stage().is_some() => {
                let ctx = self.ctx.clone();
                self.status.on_command(cmd, &ctx);
            }
            Command::CloseDetails if matches!(self.focus, PaneId::Graph | PaneId::Details) => self.close_details(),
            Command::Filter if self.focus == PaneId::Graph => self.open_graph_filter(),
            // The graph has no tabs of its own; step the branches pane's.
            Command::NextTab | Command::PrevTab if self.focus == PaneId::Graph => {
                let ctx = self.ctx.clone();
                self.branches.on_command(cmd, &ctx);
            }
            Command::Refresh => self.update(Action::Refresh),
            // ctrl+f fetches from any pane; f does too in the branches pane.
            Command::Fetch => self.update(Action::Run(GitOp::FetchAll)),
            Command::Palette => {
                // In blame, a number typed there goes to that line, as `:42` in vim.
                let lines = self.blame_line_count();
                self.modal = Some(Modal::palette(self.palette_items()).with_lines(lines));
            }
            Command::GoToLine => match self.blame_line_count() {
                0 => {}
                lines => self.modal = Some(Modal::go_to_line(lines)),
            },
            Command::PluginLogs => self.show_plugin_logs(),
            Command::SelfUpdate => self.check_update(),
            Command::OpenLink if self.focus == PaneId::Details => self.open_link(),
            Command::ToggleFiles if self.focus == PaneId::Details => self.toggle_files(),
            Command::Continue | Command::Abort if self.focus != PaneId::Status => {
                let ctx = self.ctx.clone();
                self.status.on_command(cmd, &ctx);
            }
            Command::Push => self.update(Action::Run(GitOp::Push)),
            Command::Pull => self.update(Action::Run(GitOp::Pull)),
            Command::Undo => self.plan_undo(),
            Command::Agent => self.open_agent_dialog(),
            _ => {
                let ctx = self.ctx.clone();
                if self.focused_mut().on_command(cmd, &ctx) == EventState::NotConsumed {
                    tracing::debug!(?cmd, pane = ?self.focus, "command not handled");
                }
            }
        }
    }

    /// Moves focus to the nearest pane on screen in the direction of `cmd`
    /// (one of the `Focus{Left,Down,Up,Right}` commands), vim-window style.
    /// Panes that overlap the focused one across that direction win; ties go
    /// to the one whose middle is closest.
    fn focus_toward(&mut self, cmd: Command) {
        let Some(from) = self.pane_area(self.focus) else {
            return;
        };
        let sideways = matches!(cmd, Command::FocusLeft | Command::FocusRight);
        // A rect's extent across the direction of travel, as (start, end).
        let across = |r: Rect| if sideways { (r.y, r.bottom()) } else { (r.x, r.right()) };
        // Sorts best first: overlapping across, then nearest, then most centred.
        let score = |to: Rect| -> Option<(bool, u16, u32)> {
            let gap = match cmd {
                Command::FocusLeft => from.x.checked_sub(to.right())?,
                Command::FocusRight => to.x.checked_sub(from.right())?,
                Command::FocusUp => from.y.checked_sub(to.bottom())?,
                Command::FocusDown => to.y.checked_sub(from.bottom())?,
                _ => return None,
            };
            let ((a0, a1), (b0, b1)) = (across(from), across(to));
            let overlaps = a0 < b1 && b0 < a1;
            let off_centre = (u32::from(a0) + u32::from(a1)).abs_diff(u32::from(b0) + u32::from(b1));
            Some((!overlaps, gap, off_centre))
        };
        let next = self
            .pane_areas
            .iter()
            .filter(|(p, _)| *p != self.focus)
            .filter_map(|(p, r)| score(*r).map(|s| (s, *p)))
            .min_by_key(|(s, _)| *s);
        if let Some((_, pane)) = next {
            self.focus = pane;
        }
    }

    /// Moves focus `step` places round [`PaneId::ALL`], skipping hidden panes.
    fn cycle_focus(&mut self, step: usize) {
        let idx = PaneId::ALL.iter().position(|p| *p == self.focus).unwrap_or(0);
        let n = PaneId::ALL.len();
        if let Some(next) = (1..n)
            .map(|i| PaneId::ALL[(idx + i * step) % n])
            .find(|p| self.shows(*p))
        {
            self.focus = next;
        }
    }

    fn focused(&self) -> &dyn Component {
        match self.focus {
            PaneId::Status => &self.status,
            PaneId::Branches => &self.branches,
            PaneId::Graph => &self.graph,
            PaneId::Details => &self.details,
        }
    }

    fn focused_mut(&mut self) -> &mut dyn Component {
        match self.focus {
            PaneId::Status => &mut self.status,
            PaneId::Branches => &mut self.branches,
            PaneId::Graph => &mut self.graph,
            PaneId::Details => &mut self.details,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.cursor_shown = false;
        if self.notice != self.notice_drawn.0 {
            self.notice_drawn = (self.notice.clone(), self.ticks);
        }
        let [main, help] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        self.pane_areas = match self.zoom {
            Zoom::Full(pane) => vec![(pane, main)],
            _ => self.config.layout.place(main, self.view()),
        };
        // Where details would open, for sizing split plugin panes while it's closed.
        let details_area = self.pane_area(PaneId::Details).unwrap_or_else(|| {
            let open = View {
                details: true,
                ..self.view()
            };
            let placed = self.config.layout.place(main, open);
            placed
                .into_iter()
                .find(|(p, _)| *p == PaneId::Details)
                .map_or(main, |(_, r)| r)
        });
        self.layout_rects = (main, details_area);
        let focus = self.focus;
        let theme = &self.theme;
        for (pane, area) in self.pane_areas.clone() {
            match pane {
                PaneId::Status => self.status.draw(frame, area, focus == pane, theme),
                PaneId::Branches => self.branches.draw(frame, area, focus == pane, theme),
                PaneId::Graph => self.graph.draw(frame, area, focus == pane, theme),
                PaneId::Details => {}
            }
        }
        if self.pane_area(PaneId::Details).is_some() && !self.draw_split_pane(frame, details_area) {
            // Fullscreen has room for the file list beside the diff.
            self.details.set_files_beside(self.zoom == Zoom::Full(PaneId::Details));
            let theme = &self.theme;
            self.details.draw(frame, details_area, focus == PaneId::Details, theme);
        }

        self.draw_help_bar(frame, help);
        if self.show_help {
            self.draw_help_popup(frame, main);
        }
        self.draw_modal_panes(frame, main, details_area);
        if let Some(modal) = &self.modal {
            modal.draw(frame, main, &self.theme);
        }
        if let Some(err) = &self.error {
            draw_error_popup(frame, main, err, &self.theme);
        }
    }

    fn draw_help_bar(&self, frame: &mut Frame, area: Rect) {
        use unicode_width::UnicodeWidthStr;

        // The notice and any running git operation get their own
        // right-aligned slot (up to half the width) so a long list of key
        // hints can't push them off screen. While something runs, a notice
        // shows beside it for a moment, then only the operation is left.
        let mut keys_area = area;
        let mut parts: Vec<(String, Style)> = Vec::new();
        if let Some(notice) = &self.notice
            && (self.busy.is_empty() || self.notice_age() < NOTICE_HOLD)
        {
            let style = if notice.starts_with('✓') && self.notice_age() < SUCCESS_FADE {
                self.theme.diff_add
            } else {
                self.theme.muted
            };
            parts.push((notice.clone(), style));
        }
        if let Some(line) = self.busy_line() {
            if !parts.is_empty() {
                parts.push((" · ".into(), self.theme.muted));
            }
            parts.push((line, self.theme.help_key.add_modifier(Modifier::BOLD)));
        }
        if !parts.is_empty() {
            parts.insert(0, (" │ ".into(), self.theme.muted));
            let text: String = parts.iter().map(|(t, _)| t.as_str()).collect();
            let width = (text.width() as u16).min(area.width / 2);
            let [left, right] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(width)]).areas(area);
            keys_area = left;
            frame.render_widget(Paragraph::new(truncate_spans(parts, width as usize)), right);
        }

        // Global hints always show; pane hints fill what's left, in order,
        // skipping any that don't fit (the full list is under `?`).
        let context = self.focused().context();
        let hint = |cmd: &Command| {
            self.keymap
                .display_key_text(context, *cmd)
                .map(|key| (key, cmd.label()))
        };
        // A global whose key the pane uses for something else isn't shown.
        let globals: Vec<_> = GLOBAL_HELP
            .iter()
            .filter(|cmd| {
                self.keymap
                    .display_key(context, **cmd)
                    .is_some_and(|key| self.keymap.lookup(context, key) == Some(**cmd))
            })
            .filter_map(hint)
            .collect();
        let width_of = |(k, l): &(&str, &str)| k.width() + 1 + l.width() + 2;
        let mut budget = (keys_area.width as usize).saturating_sub(globals.iter().map(width_of).sum());
        let mut items = Vec::new();
        for item in self.focused().commands().iter().filter_map(hint) {
            if width_of(&item) <= budget {
                budget -= width_of(&item);
                items.push(item);
            }
        }
        items.extend(globals);
        let mut spans = Vec::new();
        for (key, label) in items {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(key, self.theme.help_key));
            spans.push(Span::styled(format!(" {label}"), self.theme.help_text));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), keys_area);
    }

    fn draw_help_popup(&self, frame: &mut Frame, area: Rect) {
        let search = if self.help_query.is_empty() {
            Line::from(vec![
                Span::styled("> ", self.theme.help_key),
                Span::styled("type to search", self.theme.muted),
            ])
        } else {
            Line::from(vec![
                Span::styled("> ", self.theme.help_key),
                Span::raw(self.help_query.clone()),
                Span::styled("█", self.theme.help_key),
            ])
        };
        let mut lines = vec![search];
        let rows = self.help_rows();
        if rows.is_empty() {
            lines.push(Line::styled("no matching keys", self.theme.muted));
        }
        for (keys, label) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("{keys:>16}  "), self.theme.help_key),
                Span::styled(label, self.theme.help_text),
            ]));
        }

        // Size for the unfiltered list so the popup holds still while typing.
        let popup = centered(area, 50, self.all_help_row_count() as u16 + 3);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_focused)
            .title(Span::styled(" Keybindings ", self.theme.title_focused))
            .title_top(
                Line::styled(concat!(" tt-git ", env!("CARGO_PKG_VERSION"), " "), self.theme.muted).right_aligned(),
            );
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }
}

fn draw_error_popup(frame: &mut Frame, area: Rect, msg: &str, theme: &Theme) {
    let width = 70.min(area.width);
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let text_lines: usize = msg
        .lines()
        .map(|l| l.chars().count().div_ceil(inner_width).max(1))
        .sum();
    let popup = centered(area, width, text_lines as u16 + 2);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.error)
        .title(Span::styled(" Error ", theme.error));
    frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }).block(block), popup);
}

/// A revision as a title shows it: a commit id shortened, keeping a `^`.
fn short_rev(rev: &str) -> String {
    let (base, suffix) = rev.split_at(rev.find(['^', '~']).unwrap_or(rev.len()));
    let short = if base.len() >= 40 && base.bytes().all(|b| b.is_ascii_hexdigit()) {
        &base[..7]
    } else {
        base
    };
    format!("{short}{suffix}")
}

pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
