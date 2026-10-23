use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tt_git_core::branches::{matches_filter, tags_newest_first};
use tt_git_core::time::{SharedClock, relative};

use tt_git_core::{BaseBranches, BranchEntry, BranchState, GitRef, RefKind, Stash, Worktree};
use unicode_width::UnicodeWidthStr;

use super::{BranchFocus, DrawnDates, PaneId, pane_block, title_x};
use crate::action::Action;
use crate::component::{Component, EventState};
use crate::jobs::Ctx;
use crate::keymap::{Command, Context, KeyCombo};
use crate::modal::{GitOp, Modal, PaletteItem, PaletteTarget, PromptAction};
use crate::theme::Theme;
use crate::widgets::{Cursor, fit, fit_right};

const STATE_WIDTH: usize = 9;
const DATE_WIDTH: usize = 4;
const AUTHOR_WIDTH: usize = 14;
/// Narrowest commit subject column worth showing in tag and merged/unmerged rows.
const SUBJECT_MIN_WIDTH: usize = 12;
/// Widest a plugin's decoration column gets.
pub const DECORATION_WIDTH: usize = 10;

/// The pane's lists, switched lazygit-style with `[` and `]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchTab {
    /// Every local and remote branch, by recent activity.
    #[default]
    All,
    /// Tags, newest first.
    Tags,
    /// Branches on the base's remote that aren't merged into it.
    Unmerged,
    /// Branches on the base's remote that are merged into it.
    Merged,
    /// Stashes, newest first.
    Stash,
    /// Every working tree of the repository, the main one first.
    Worktrees,
}

impl BranchTab {
    pub const ALL: [BranchTab; 6] = [
        BranchTab::All,
        BranchTab::Tags,
        BranchTab::Unmerged,
        BranchTab::Merged,
        BranchTab::Stash,
        BranchTab::Worktrees,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BranchTab::All => "Branches",
            BranchTab::Tags => "Tags",
            BranchTab::Stash => "Stash",
            BranchTab::Unmerged => "Unmerged",
            BranchTab::Merged => "Merged",
            BranchTab::Worktrees => "Worktrees",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The tabs that compare branches with the base branch.
    fn compares(self) -> bool {
        matches!(self, BranchTab::Unmerged | BranchTab::Merged)
    }
}

/// Unified local + remote branch list sorted by recent activity, the tags,
/// and the branches merged and not merged into the base branch.
#[derive(Debug)]
pub struct BranchesPane {
    clock: SharedClock,
    entries: Vec<BranchEntry>,
    base: Option<BaseBranches>,
    /// Newest first.
    tags: Vec<GitRef>,
    /// Newest first.
    stashes: Vec<Stash>,
    /// The main worktree first.
    worktrees: Vec<Worktree>,
    /// How each worktree is shown and filtered: its path from the current
    /// directory.
    worktree_names: Vec<String>,
    /// Files with changes in each worktree, by path, once counted (`Err`
    /// when git couldn't tell).
    worktree_changes: HashMap<PathBuf, Result<usize, String>>,
    /// Commits each worktree's HEAD has that the base doesn't, by path,
    /// once counted.
    worktree_ahead: HashMap<PathBuf, u32>,
    /// Bytes each worktree's folder takes, by path, once measured (`Err`
    /// when it couldn't be read).
    worktree_sizes: HashMap<PathBuf, Result<u64, String>>,
    tab: BranchTab,
    /// Indices into the current tab's list that pass the filter.
    visible: Vec<usize>,
    filter: String,
    editing_filter: bool,
    /// One per tab, so switching back returns to the same row.
    cursors: [Cursor; BranchTab::ALL.len()],
    dates: DrawnDates,
    /// Where each tab's name was drawn in the title last frame: its row and
    /// columns, for clicks.
    tab_cells: Vec<(BranchTab, u16, std::ops::Range<u16>)>,
    /// Short text plugins put next to branches: by plugin id, then by
    /// branch name. Each plugin gets a column in the full list.
    decorations: BTreeMap<String, HashMap<String, String>>,
}

impl BranchesPane {
    pub fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            entries: Vec::new(),
            base: None,
            tags: Vec::new(),
            stashes: Vec::new(),
            worktrees: Vec::new(),
            worktree_names: Vec::new(),
            worktree_changes: HashMap::new(),
            worktree_ahead: HashMap::new(),
            worktree_sizes: HashMap::new(),
            tab: BranchTab::default(),
            visible: Vec::new(),
            filter: String::new(),
            editing_filter: false,
            cursors: [Cursor::default(); BranchTab::ALL.len()],
            dates: DrawnDates::default(),
            tab_cells: Vec::new(),
            decorations: BTreeMap::new(),
        }
    }

    /// Replaces `plugin`'s decorations; an empty map removes its column.
    pub fn set_decorations(&mut self, plugin: &str, decorations: HashMap<String, String>) {
        if decorations.is_empty() {
            self.decorations.remove(plugin);
        } else {
            self.decorations.insert(plugin.to_string(), decorations);
        }
    }

    /// Drops the decorations of every plugin `keep` returns false for.
    pub fn retain_decorations(&mut self, keep: impl Fn(&str) -> bool) {
        self.decorations.retain(|plugin, _| keep(plugin));
    }

    /// Each plugin's decoration for `branch`.
    pub fn decorations_for(&self, branch: &str) -> BTreeMap<&str, &str> {
        self.decorations
            .iter()
            .filter_map(|(plugin, map)| Some((plugin.as_str(), map.get(branch)?.as_str())))
            .collect()
    }

    /// The decoration columns of the full list: each plugin with a value for
    /// a listed branch, and the widest of those values.
    fn decoration_columns(&self) -> Vec<(&str, usize)> {
        self.decorations
            .iter()
            .filter_map(|(plugin, map)| {
                let widest = self
                    .entries
                    .iter()
                    .filter_map(|e| map.get(&e.name))
                    .map(|text| text.width())
                    .max()?;
                Some((plugin.as_str(), widest.min(DECORATION_WIDTH)))
            })
            .filter(|(_, width)| *width > 0)
            .collect()
    }

    /// Whether a relative date drawn last frame would read differently at `now`.
    pub fn dates_stale(&self, now: i64) -> bool {
        self.dates.stale(now)
    }

    /// Replaces the lists, keeping the selection on the same row if it
    /// still exists. `refs` may hold any refs; only its tags are listed.
    pub fn set_entries(
        &mut self,
        entries: Vec<BranchEntry>,
        base: Option<BaseBranches>,
        refs: &[GitRef],
        stashes: Vec<Stash>,
        worktrees: Vec<Worktree>,
    ) {
        let keep = self.selected_name().map(str::to_string);
        self.entries = entries;
        self.base = base;
        self.tags = tags_newest_first(refs);
        self.stashes = stashes;
        let base = names_base(&worktrees);
        self.worktree_names = worktrees
            .iter()
            .map(|w| tt_git_core::repo::relative_path(&w.path, &base))
            .collect();
        self.worktrees = worktrees;
        self.refilter();
        let pos = keep.and_then(|name| (0..self.visible.len()).position(|pos| self.name_at(pos) == name));
        if let Some(pos) = pos {
            self.cursor_mut().selected = pos;
        }
    }

    /// The unified list of every branch.
    pub fn entries(&self) -> &[BranchEntry] {
        &self.entries
    }

    pub fn base(&self) -> Option<&BaseBranches> {
        self.base.as_ref()
    }

    pub fn tab(&self) -> BranchTab {
        self.tab
    }

    /// The current tab's branches passing the filter, in display order
    /// (none on the tags tab).
    pub fn visible(&self) -> impl Iterator<Item = &BranchEntry> {
        let list = self.list(self.tab);
        self.visible.iter().filter_map(move |&i| list.get(i))
    }

    pub fn selected(&self) -> Option<&BranchEntry> {
        self.visible
            .get(self.cursor().selected)
            .and_then(|&i| self.list(self.tab).get(i))
    }

    /// The tags passing the filter, in display order, on the tags tab.
    pub fn visible_tags(&self) -> impl Iterator<Item = &GitRef> {
        let tags = self.tag_list();
        self.visible.iter().filter_map(move |&i| tags.get(i))
    }

    pub fn selected_tag(&self) -> Option<&GitRef> {
        self.visible
            .get(self.cursor().selected)
            .and_then(|&i| self.tag_list().get(i))
    }

    /// The stashes passing the filter, in display order, on the stash tab.
    pub fn visible_stashes(&self) -> impl Iterator<Item = &Stash> {
        let stashes = self.stash_list();
        self.visible.iter().filter_map(move |&i| stashes.get(i))
    }

    pub fn selected_stash(&self) -> Option<&Stash> {
        self.visible
            .get(self.cursor().selected)
            .and_then(|&i| self.stash_list().get(i))
    }

    /// Every worktree, the main one first.
    pub fn worktrees(&self) -> &[Worktree] {
        &self.worktrees
    }

    /// The worktrees passing the filter, in display order, on the worktrees tab.
    pub fn visible_worktrees(&self) -> impl Iterator<Item = &Worktree> {
        let worktrees = self.worktree_list();
        self.visible.iter().filter_map(move |&i| worktrees.get(i))
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        self.visible
            .get(self.cursor().selected)
            .and_then(|&i| self.worktree_list().get(i))
    }

    /// The worktrees whose changes can be counted: those with a folder.
    pub fn countable_worktrees(&self) -> Vec<PathBuf> {
        self.worktrees
            .iter()
            .filter(|w| !w.bare && w.prunable.is_none())
            .map(|w| w.path.clone())
            .collect()
    }

    /// [`countable_worktrees`](Self::countable_worktrees) with the commit
    /// each has checked out.
    pub fn countable_worktree_heads(&self) -> Vec<(PathBuf, Option<String>)> {
        self.worktrees
            .iter()
            .filter(|w| !w.bare && w.prunable.is_none())
            .map(|w| (w.path.clone(), w.head.clone()))
            .collect()
    }

    /// Takes counts of each worktree's changed files and of its commits the
    /// base doesn't have.
    pub fn set_worktree_changes(&mut self, counts: Vec<(PathBuf, Result<usize, String>, Option<u32>)>) {
        self.worktree_ahead = counts
            .iter()
            .filter_map(|(path, _, ahead)| Some((path.clone(), (*ahead)?)))
            .collect();
        self.worktree_changes = counts.into_iter().map(|(path, changes, _)| (path, changes)).collect();
    }

    /// Takes the disk space the folder of the worktree at `path` takes.
    pub fn set_worktree_size(&mut self, path: PathBuf, size: Result<u64, String>) {
        self.worktree_sizes.insert(path, size);
    }

    /// What a worktree row says about its size: `…` until measured, nothing
    /// for one without a folder.
    fn worktree_size(&self, wt: &Worktree) -> String {
        if wt.bare || wt.prunable.is_some() {
            return String::new();
        }
        match self.worktree_sizes.get(&wt.path) {
            Some(Ok(bytes)) => human_size(*bytes),
            Some(Err(_)) => "?".into(),
            None => "…".into(),
        }
    }

    /// The disk space the worktrees take together, `…` until each one with
    /// a folder is measured. Nested ones aren't counted twice: the main
    /// worktree's size leaves them out.
    fn worktrees_total(&self) -> String {
        let mut total = 0;
        for path in self.countable_worktrees() {
            match self.worktree_sizes.get(&path) {
                Some(Ok(bytes)) => total += bytes,
                Some(Err(_)) => {}
                None => return "…".into(),
            }
        }
        human_size(total)
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    fn list(&self, tab: BranchTab) -> &[BranchEntry] {
        match (tab, &self.base) {
            (BranchTab::All, _) => &self.entries,
            (BranchTab::Unmerged, Some(base)) => &base.unmerged,
            (BranchTab::Merged, Some(base)) => &base.merged,
            (BranchTab::Tags | BranchTab::Stash | BranchTab::Worktrees, _) | (_, None) => &[],
        }
    }

    fn worktree_list(&self) -> &[Worktree] {
        if self.tab == BranchTab::Worktrees {
            &self.worktrees
        } else {
            &[]
        }
    }

    fn tag_list(&self) -> &[GitRef] {
        if self.tab == BranchTab::Tags { &self.tags } else { &[] }
    }

    fn stash_list(&self) -> &[Stash] {
        if self.tab == BranchTab::Stash {
            &self.stashes
        } else {
            &[]
        }
    }

    /// How many rows `tab` has before filtering.
    fn len(&self, tab: BranchTab) -> usize {
        match tab {
            BranchTab::Tags => self.tags.len(),
            BranchTab::Stash => self.stashes.len(),
            BranchTab::Worktrees => self.worktrees.len(),
            _ => self.list(tab).len(),
        }
    }

    /// Name of the current tab's row `i` (an index into its unfiltered list).
    fn row_name(&self, i: usize) -> &str {
        match self.tab {
            BranchTab::Tags => &self.tags[i].short_name,
            BranchTab::Stash => &self.stashes[i].message,
            BranchTab::Worktrees => &self.worktree_names[i],
            _ => &self.list(self.tab)[i].name,
        }
    }

    /// Name of the visible row at `pos`.
    fn name_at(&self, pos: usize) -> &str {
        self.row_name(self.visible[pos])
    }

    fn selected_name(&self) -> Option<&str> {
        (self.cursor().selected < self.visible.len()).then(|| self.name_at(self.cursor().selected))
    }

    fn cursor(&self) -> &Cursor {
        &self.cursors[self.tab.index()]
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cursors[self.tab.index()]
    }

    fn refilter(&mut self) {
        self.visible = (0..self.len(self.tab))
            .filter(|&i| matches_filter(self.row_name(i), &self.filter))
            .collect();
        let len = self.visible.len();
        self.cursor_mut().clamp(len);
    }

    /// Moves `step` tabs along, wrapping round and skipping hidden ones.
    fn switch_tab(&mut self, step: usize, ctx: &Ctx) {
        let n = BranchTab::ALL.len();
        let mut next = self.tab;
        loop {
            next = BranchTab::ALL[(next.index() + step) % n];
            if self.shows_tab(next) {
                break;
            }
        }
        self.select_tab(next, ctx);
    }

    fn select_tab(&mut self, tab: BranchTab, ctx: &Ctx) {
        self.tab = tab;
        self.refilter();
        self.announce(ctx);
        if tab == BranchTab::Worktrees {
            ctx.emit(Action::CountWorktreeChanges);
        }
    }

    /// The Stash tab only shows while there are stashes, and the Worktrees
    /// tab while there's more than the main one (or while it's current).
    fn shows_tab(&self, tab: BranchTab) -> bool {
        match tab {
            _ if tab == self.tab => true,
            BranchTab::Stash => !self.stashes.is_empty(),
            BranchTab::Worktrees => self.worktrees.len() > 1,
            _ => true,
        }
    }

    /// On the merged and unmerged tabs, the selected branch for the graph
    /// to show on its own.
    pub fn focus(&self) -> Option<BranchFocus> {
        if !self.tab.compares() {
            return None;
        }
        let base = self.base.as_ref()?;
        let tip = BaseBranches::tip(self.selected()?)?;
        Some(BranchFocus {
            branch: tip.short_name.clone(),
            tip: tip.target.clone(),
            base: base.base.clone(),
            base_ref: base.base_ref.clone(),
        })
    }

    fn announce(&self, ctx: &Ctx) {
        ctx.emit(Action::BranchFocused(self.focus()));
        if let Some(stash) = self.selected_stash() {
            ctx.emit(Action::StashSelected(stash.clone()));
        }
        let oid = match self.tab {
            BranchTab::All => self.selected().and_then(BranchEntry::target),
            BranchTab::Tags => self.selected_tag().map(|t| t.target.as_str()),
            BranchTab::Worktrees => self.selected_worktree().and_then(|w| w.head.as_deref()),
            _ => None,
        };
        if let Some(oid) = oid {
            ctx.emit(Action::BranchSelected(oid.to_string()));
        }
    }

    /// The base branch picker: detect automatically, then every remote
    /// branch, then every local one.
    fn base_picker(&self) -> Modal {
        let current = self.base.as_ref().map(|b| b.base.as_str());
        let detected = self.base.as_ref().and_then(|b| b.detected.clone());
        let mut items = vec![PaletteItem {
            label: "Detect automatically".into(),
            hint: detected.unwrap_or_else(|| "none found".into()),
            target: PaletteTarget::Op(GitOp::SetBaseBranch(None)),
        }];
        let remotes = self.entries.iter().flat_map(|e| e.remotes.iter());
        let locals = self.entries.iter().filter_map(|e| e.local.as_ref());
        for r in remotes.chain(locals) {
            let hint = if Some(r.short_name.as_str()) == current {
                "current"
            } else {
                ""
            };
            items.push(PaletteItem {
                label: r.short_name.clone(),
                hint: hint.into(),
                target: PaletteTarget::Op(GitOp::SetBaseBranch(Some(r.short_name.clone()))),
            });
        }
        let selected = match &self.base {
            Some(b) if b.chosen => items.iter().position(|i| i.label == b.base).unwrap_or(0),
            _ => 0,
        };
        Modal::picker("Base branch", items, selected)
    }

    fn start_ref(entry: &BranchEntry) -> String {
        entry
            .local
            .as_ref()
            .or(entry.remotes.first())
            .map(|r| r.short_name.clone())
            .unwrap_or_else(|| entry.name.clone())
    }

    fn delete_modal(entry: &BranchEntry) -> Option<Modal> {
        if let Some(local) = &entry.local {
            if entry.is_head {
                return None;
            }
            let unmerged_note = "\n(If it is not fully merged you will be asked to force-delete.)";
            return Some(Modal::confirm(
                "Delete branch",
                format!("Delete local branch {}?{unmerged_note}", local.short_name),
                GitOp::DeleteBranch {
                    name: local.short_name.clone(),
                    force: false,
                },
            ));
        }
        let remote_ref = entry.remotes.first()?;
        let RefKind::RemoteBranch { remote } = &remote_ref.kind else {
            return None;
        };
        Some(Modal::confirm(
            "Delete remote branch",
            format!(
                "Delete {} on remote {remote}?\nThis runs `git push {remote} --delete {}`.",
                remote_ref.short_name,
                remote_ref.branch_name()
            ),
            GitOp::DeleteRemoteBranch {
                remote: remote.clone(),
                branch: remote_ref.branch_name().to_string(),
            },
        ))
    }

    /// The remote tags are pushed to and deleted from: `origin` if there is
    /// one, otherwise the first remote with branches.
    fn tag_remote(&self) -> Option<String> {
        let mut remotes = self
            .entries
            .iter()
            .flat_map(|e| e.remotes.iter())
            .filter_map(|r| match &r.kind {
                RefKind::RemoteBranch { remote } => Some(remote.as_str()),
                _ => None,
            });
        let first = remotes.next()?;
        let preferred = std::iter::once(first)
            .chain(remotes)
            .find(|r| *r == tt_git_core::branches::PREFERRED_REMOTE);
        Some(preferred.unwrap_or(first).to_string())
    }

    /// Asks before deleting tag `name`: `y` here only, or with a remote,
    /// `r` there too.
    fn delete_tag_modal(&self, name: String) -> Modal {
        let local = GitOp::DeleteTag {
            name: name.clone(),
            remote: None,
        };
        let Some(remote) = self.tag_remote() else {
            return Modal::confirm("Delete tag", format!("Delete local tag {name}?"), local);
        };
        Modal::confirm(
            "Delete tag",
            format!(
                "Delete local tag {name}?
r also deletes it on {remote} (`git push {remote} --delete refs/tags/{name}`)."
            ),
            local,
        )
        .confirm_or(
            'r',
            format!("here and on {remote}"),
            GitOp::DeleteTag {
                name,
                remote: Some(remote),
            },
        )
    }

    /// Commands that act on the selected tag.
    fn on_tag_command(&self, cmd: Command, ctx: &Ctx) -> EventState {
        let Some(tag) = self.selected_tag() else {
            return EventState::NotConsumed;
        };
        let name = tag.short_name.clone();
        match cmd {
            Command::Checkout => ctx.emit(Action::Notify(format!("n makes a branch from {name}"))),
            Command::NewBranch => ctx.emit(Action::OpenModal(Modal::prompt(
                format!("New branch from {name}"),
                "",
                PromptAction::CreateBranch {
                    start: name,
                    worktree: false,
                },
            ))),
            Command::Merge => ctx.emit(Action::OpenModal(Modal::confirm(
                "Merge",
                format!("Merge {name} into the current branch?"),
                GitOp::Merge { target: name },
            ))),
            Command::Rebase => ctx.emit(Action::OpenModal(Modal::confirm(
                "Rebase",
                format!("Rebase the current branch onto {name}?"),
                GitOp::Rebase { onto: name },
            ))),
            Command::DeleteBranch => ctx.emit(Action::OpenModal(self.delete_tag_modal(name))),
            Command::RenameBranch => ctx.emit(Action::Notify("only local branches can be renamed".into())),
            Command::NewTag => ctx.emit(Action::OpenModal(Modal::new_tag(&name, tag.target.clone()))),
            Command::PushTag => ctx.emit(Action::Run(GitOp::PushTag { name })),
            Command::Copy => ctx.emit(Action::Copy(name)),
            _ => return EventState::NotConsumed,
        }
        EventState::Consumed
    }

    /// Commands that act on the selected stash.
    fn on_stash_command(&self, cmd: Command, ctx: &Ctx) -> EventState {
        let Some(stash) = self.selected_stash() else {
            return EventState::NotConsumed;
        };
        let (index, name) = (stash.index, stash.name());
        match cmd {
            Command::StashPop | Command::StashApply => ctx.emit(Action::Run(GitOp::StashApply {
                index,
                pop: cmd == Command::StashPop,
            })),
            Command::StashDrop => ctx.emit(Action::OpenModal(Modal::confirm(
                "Drop stash",
                format!("Drop {name} ({})? This cannot be undone.", stash.message),
                GitOp::StashDrop { index },
            ))),
            Command::NewBranch => ctx.emit(Action::OpenModal(Modal::prompt(
                format!("New branch from {name}"),
                "",
                PromptAction::StashBranch { index },
            ))),
            _ => return EventState::NotConsumed,
        }
        EventState::Consumed
    }

    /// A stash row: its name, message and age.
    fn render_stash<'a>(&self, stash: &'a Stash, width: usize, theme: &Theme) -> Line<'a> {
        let name = stash.name();
        let message_width = width.saturating_sub(2 + name.width() + 1 + 1 + DATE_WIDTH);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(name, theme.sha),
            Span::raw(" "),
            Span::raw(fit(&stash.message, message_width)),
            Span::raw(" "),
            Span::styled(
                fit_right(&relative(self.clock.now(), stash.date), DATE_WIDTH),
                theme.date,
            ),
        ])
    }

    /// Commands that act on the selected worktree.
    fn on_worktree_command(&self, cmd: Command, ctx: &Ctx) -> EventState {
        let Some(wt) = self.selected_worktree() else {
            return EventState::NotConsumed;
        };
        let name = self.name_at(self.cursor().selected).to_string();
        let notify = |msg: String| ctx.emit(Action::Notify(msg));
        match cmd {
            Command::OpenWorktree => {
                if wt.current {
                    notify("tt-git is already in this worktree".into());
                } else if wt.bare {
                    notify("that's the bare repository; it has no working tree to open".into());
                } else if wt.prunable.is_some() {
                    notify(format!("{name}'s folder is gone (d drops it from the list)"));
                } else {
                    ctx.emit(Action::OpenWorktree(wt.path.clone()));
                }
            }
            Command::RemoveWorktree => match remove_modal(wt, &name) {
                Ok(modal) => ctx.emit(Action::OpenModal(modal)),
                Err(why) => notify(why),
            },
            Command::NewBranch => match wt.branch.clone().or_else(|| wt.head.clone()) {
                Some(start) => {
                    let shown = wt
                        .branch
                        .clone()
                        .unwrap_or_else(|| start[..start.len().min(7)].to_string());
                    ctx.emit(Action::OpenModal(Modal::prompt(
                        format!("New branch from {shown}"),
                        "",
                        PromptAction::CreateBranch { start, worktree: true },
                    )))
                }
                None => notify(format!("{name} has no commit to start from")),
            },
            // The absolute path, with Windows' own separators where git
            // printed forward slashes.
            Command::Copy => ctx.emit(Action::Copy(if cfg!(windows) {
                wt.path.display().to_string().replace('/', "\\")
            } else {
                wt.path.display().to_string()
            })),
            _ => return EventState::NotConsumed,
        }
        EventState::Consumed
    }

    /// What a worktree row says about its state: main, locked, missing,
    /// `↑n` for commits the base doesn't have, and how many files have
    /// changes once that's counted.
    fn worktree_state(&self, wt: &Worktree) -> String {
        let mut parts = Vec::new();
        if wt.main {
            parts.push("main".to_string());
        }
        if wt.locked.is_some() {
            parts.push("locked".into());
        }
        if wt.prunable.is_some() {
            parts.push("missing".into());
        } else if !wt.bare {
            if let Some(&ahead) = self.worktree_ahead.get(&wt.path)
                && ahead > 0
            {
                parts.push(format!("↑{ahead}"));
            }
            parts.push(match self.worktree_changes.get(&wt.path) {
                Some(Ok(0)) => "clean".into(),
                Some(Ok(n)) => format!("{n} changed"),
                Some(Err(_)) => "?".into(),
                None => "…".into(),
            });
        }
        parts.join(" · ")
    }

    /// A worktree row: its path, what it has checked out and its state.
    fn render_worktree<'a>(
        &self,
        i: usize,
        name_width: usize,
        state_width: usize,
        size_width: usize,
        width: usize,
        theme: &Theme,
    ) -> Line<'a> {
        let wt = &self.worktrees[i];
        let (marker, name_style) = if wt.current {
            (Span::styled("* ", theme.label_local), theme.label_local)
        } else {
            (Span::raw("  "), Style::new())
        };
        let checked_out = match (&wt.branch, &wt.head) {
            _ if wt.bare => "(bare)".to_string(),
            (Some(branch), _) => branch.clone(),
            (None, Some(oid)) => format!("(detached {})", &oid[..oid.len().min(7)]),
            (None, None) => "(no commit)".to_string(),
        };
        let branch_style = if wt.branch.is_some() {
            theme.label_local
        } else {
            theme.sha
        };
        let state = self.worktree_state(wt);
        let state_style = if wt.prunable.is_some() {
            theme.error
        } else if wt.locked.is_some() || state.ends_with("changed") || state.contains('↑') {
            theme.label_tag
        } else {
            theme.muted
        };
        let branch_width = width.saturating_sub(2 + name_width + 1 + 1 + state_width + 1 + size_width);
        Line::from(vec![
            marker,
            Span::styled(fit(&self.worktree_names[i], name_width), name_style),
            Span::raw(" "),
            Span::styled(fit(&checked_out, branch_width), branch_style),
            Span::raw(" "),
            Span::styled(fit_right(&state, state_width), state_style),
            Span::raw(" "),
            Span::styled(fit_right(&self.worktree_size(wt), size_width), theme.muted),
        ])
    }

    /// Commands that act on the selected branch.
    fn on_entry_command(&self, cmd: Command, ctx: &Ctx) -> EventState {
        let Some(entry) = self.selected() else {
            return EventState::NotConsumed;
        };
        match cmd {
            Command::Checkout => {
                if !entry.is_head
                    && let Some(plan) = entry.checkout_plan()
                {
                    ctx.emit(Action::Run(GitOp::Checkout { plan, stash: false }));
                }
            }
            Command::NewBranch => ctx.emit(Action::OpenModal(Modal::prompt(
                format!("New branch from {}", Self::start_ref(entry)),
                "",
                PromptAction::CreateBranch {
                    start: Self::start_ref(entry),
                    worktree: false,
                },
            ))),
            Command::Merge | Command::Rebase => {
                if entry.is_head {
                    ctx.emit(Action::Notify("that's the checked-out branch".into()));
                } else {
                    let target = Self::start_ref(entry);
                    let modal = if cmd == Command::Merge {
                        Modal::confirm(
                            "Merge",
                            format!("Merge {target} into the current branch?"),
                            GitOp::Merge { target },
                        )
                    } else {
                        Modal::confirm(
                            "Rebase",
                            format!("Rebase the current branch onto {target}?"),
                            GitOp::Rebase { onto: target },
                        )
                    };
                    ctx.emit(Action::OpenModal(modal));
                }
            }
            Command::DeleteBranch => match Self::delete_modal(entry) {
                Some(modal) => ctx.emit(Action::OpenModal(modal)),
                None => ctx.emit(Action::Notify("can't delete the checked-out branch".into())),
            },
            Command::RenameBranch => match &entry.local {
                Some(local) => ctx.emit(Action::OpenModal(Modal::prompt(
                    format!("Rename {}", local.short_name),
                    local.short_name.clone(),
                    PromptAction::RenameBranch {
                        old: local.short_name.clone(),
                    },
                ))),
                None => ctx.emit(Action::Notify("only local branches can be renamed".into())),
            },
            Command::NewTag => {
                let start = Self::start_ref(entry);
                ctx.emit(Action::OpenModal(Modal::new_tag(&start, start.clone())))
            }
            // `P` pushes the selected tag on the Tags tab, the current branch here.
            Command::PushTag => ctx.emit(Action::Run(GitOp::Push)),
            Command::Copy => ctx.emit(Action::Copy(entry.name.clone())),
            _ => return EventState::NotConsumed,
        }
        EventState::Consumed
    }

    fn name_style(entry: &BranchEntry, theme: &Theme) -> Style {
        if entry.is_head {
            theme.label_local
        } else if entry.local.is_none() {
            theme.label_remote
        } else {
            Style::new()
        }
    }

    fn marker(entry: &BranchEntry, theme: &Theme) -> Span<'static> {
        if entry.is_head {
            Span::styled("* ", theme.label_local)
        } else {
            Span::raw("  ")
        }
    }

    fn render_entry<'a>(
        &self,
        entry: &'a BranchEntry,
        columns: &[(&str, usize)],
        width: usize,
        theme: &Theme,
    ) -> Line<'a> {
        let (state, state_style) = state_text(&entry.state(), theme);
        let decorations: usize = columns.iter().map(|(_, w)| w + 1).sum();
        let name_width = width.saturating_sub(2 + 1 + STATE_WIDTH + 1 + DATE_WIDTH + decorations);
        let mut spans = vec![
            Self::marker(entry, theme),
            Span::styled(fit(&entry.name, name_width), Self::name_style(entry, theme)),
        ];
        for (plugin, w) in columns {
            let text = self.decorations[*plugin].get(&entry.name).map_or("", String::as_str);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fit(text, *w), theme.sha));
        }
        spans.extend([
            Span::raw(" "),
            Span::styled(fit(&state, STATE_WIDTH), state_style),
            Span::raw(" "),
            Span::styled(
                fit_right(&relative(self.clock.now(), entry.activity), DATE_WIDTH),
                theme.date,
            ),
        ]);
        Line::from(spans)
    }

    /// A merged/unmerged row: name, tip subject, author and commit date,
    /// like a `git branch -r --no-merged` alias prints them.
    fn render_compared<'a>(
        &'a self,
        entry: &'a BranchEntry,
        name_width: usize,
        width: usize,
        theme: &Theme,
    ) -> Line<'a> {
        let tip = BaseBranches::tip(entry);
        let author = tip
            .and_then(|t| self.base.as_ref()?.authors.get(&t.full_name))
            .map_or("", String::as_str);
        let mut spans = vec![
            Self::marker(entry, theme),
            Span::styled(fit(&entry.name, name_width), Self::name_style(entry, theme)),
        ];
        let rest = width.saturating_sub(2 + name_width + 1 + DATE_WIDTH);
        let mut author_width = AUTHOR_WIDTH.min(rest.saturating_sub(1));
        let subject_width = rest.saturating_sub(author_width + 2);
        if subject_width < SUBJECT_MIN_WIDTH {
            author_width = rest.saturating_sub(1);
        } else {
            spans.push(Span::raw(" "));
            spans.push(Span::raw(fit(tip.map_or("", |t| t.subject.as_str()), subject_width)));
        }
        if author_width > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fit(author, author_width), theme.muted));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            fit_right(&relative(self.clock.now(), entry.activity), DATE_WIDTH),
            theme.date,
        ));
        Line::from(spans)
    }

    /// How far a tag is from the base: `↓12` when the base has moved on 12
    /// commits, `↑3` for commits the base doesn't have, `✓` at its tip.
    /// Only the newest tags are compared.
    fn tag_distance(&self, tag: &GitRef, theme: &Theme) -> Option<(String, Style)> {
        let d = self.base.as_ref()?.tags.get(&tag.full_name)?;
        let state = if d.ahead == 0 && d.behind == 0 {
            BranchState::InSync
        } else {
            BranchState::Diverged {
                ahead: d.ahead,
                behind: d.behind,
            }
        };
        Some(state_text(&state, theme))
    }

    /// Width of the tag distance column: the widest of the compared tags.
    fn tag_distance_width(&self, theme: &Theme) -> usize {
        self.tags
            .iter()
            .filter_map(|t| self.tag_distance(t, theme))
            .map(|(text, _)| text.width())
            .max()
            .unwrap_or(0)
    }

    /// A tag row: name, distance from the base, subject (the tag message's
    /// for annotated tags, else the commit's) and date.
    fn render_tag<'a>(
        &self,
        tag: &'a GitRef,
        name_width: usize,
        distance_width: usize,
        width: usize,
        theme: &Theme,
    ) -> Line<'a> {
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(fit(&tag.short_name, name_width), theme.label_tag),
        ];
        let mut used = 2 + name_width;
        if distance_width > 0 {
            let (text, style) = self.tag_distance(tag, theme).unwrap_or_default();
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fit(&text, distance_width), style));
            used += 1 + distance_width;
        }
        let subject_width = width.saturating_sub(used + 1 + 1 + DATE_WIDTH);
        if subject_width > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::raw(fit(&tag.subject, subject_width)));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            fit_right(&relative(self.clock.now(), tag.date), DATE_WIDTH),
            theme.date,
        ));
        Line::from(spans)
    }

    /// Name column width for tag and merged/unmerged rows: the longest name on
    /// screen, up to half the row if that leaves room for subjects, else
    /// everything but the author column.
    fn compared_name_width(&self, rows: &[usize], width: usize) -> usize {
        let room = width.saturating_sub(2 + 1 + DATE_WIDTH);
        let longest = rows.iter().map(|&i| self.row_name(i).width()).max().unwrap_or(0).max(8);
        let with_subject = longest.min(room / 2);
        if room.saturating_sub(with_subject + 1 + AUTHOR_WIDTH + 1) >= SUBJECT_MIN_WIDTH {
            with_subject
        } else {
            longest.min(room.saturating_sub(AUTHOR_WIDTH + 1).max(room / 2))
        }
    }

    /// `Branches 12 · Tags 3 · Unmerged 4 · Merged 9` with the current tab
    /// underlined, then the base everywhere but the full list.
    /// The title, and the columns each tab's name takes in it.
    /// Without room for every tab's count (`counts` false), only the
    /// current tab's is shown.
    fn title(&self, theme: &Theme, counts: bool) -> (Line<'static>, Vec<(BranchTab, std::ops::Range<u16>)>) {
        let mut spans = Vec::new();
        let mut cells = Vec::new();
        let mut x = 0;
        for tab in BranchTab::ALL.into_iter().filter(|t| self.shows_tab(*t)) {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", theme.muted));
                x += 3;
            }
            let count = if (tab.compares() && self.base.is_none()) || (!counts && tab != self.tab) {
                String::new()
            } else {
                format!(" {}", self.len(tab))
            };
            let style = if tab == self.tab {
                Style::new().add_modifier(Modifier::UNDERLINED)
            } else {
                theme.muted
            };
            let text = format!("{}{count}", tab.label());
            let width = text.width() as u16;
            cells.push((tab, x..x + width));
            x += width;
            spans.push(Span::styled(text, style));
        }
        if !matches!(self.tab, BranchTab::All | BranchTab::Stash | BranchTab::Worktrees)
            && let Some(base) = &self.base
        {
            spans.push(Span::styled(format!(" ({})", base.base), theme.muted));
        }
        if self.editing_filter || !self.filter.is_empty() {
            let cursor = if self.editing_filter { "█" } else { "" };
            spans.push(Span::raw(format!(" /{}{cursor}", self.filter)));
        }
        (Line::from(spans), cells)
    }

    fn empty_message(&self) -> String {
        let base = self.base.as_ref().map(|b| b.base.as_str());
        match (self.tab, base) {
            _ if self.len(self.tab) > 0 => "no matches".into(),
            (BranchTab::All, _) => "no branches".into(),
            (BranchTab::Tags, _) => "no tags".into(),
            (BranchTab::Stash, _) => "no stashes (s in the status pane stashes changes)".into(),
            (BranchTab::Worktrees, _) => "no worktrees".into(),
            (_, None) => "no base branch found; pick one".into(),
            (BranchTab::Unmerged, Some(base)) => format!("everything is merged into {base}"),
            (BranchTab::Merged, Some(base)) => format!("nothing is merged into {base} yet"),
        }
    }
}

/// Where the worktrees tab's paths are given from: the current directory
/// when it's inside the worktree tt-git runs in, else that worktree's root.
fn names_base(worktrees: &[Worktree]) -> PathBuf {
    let Some(current) = worktrees.iter().find(|w| w.current).or(worktrees.first()) else {
        return PathBuf::new();
    };
    let inside = |cwd: &Path| match (std::fs::canonicalize(cwd), std::fs::canonicalize(&current.path)) {
        (Ok(cwd), Ok(root)) => cwd.starts_with(root),
        _ => false,
    };
    match std::env::current_dir() {
        Ok(cwd) if inside(&cwd) => cwd,
        _ => current.path.clone(),
    }
}

/// `bytes` for people: `512 B`, `4.0 KB`, `1.2 GB` (powers of 1024).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// The confirmation for removing `wt` (shown as `name`), or why it can't be.
fn remove_modal(wt: &Worktree, name: &str) -> Result<Modal, String> {
    if wt.main {
        return Err("the main worktree can't be removed".into());
    }
    if wt.current {
        return Err("tt-git is running in this worktree; open another one first".into());
    }
    if let Some(reason) = &wt.locked {
        let why = if reason.is_empty() {
            String::new()
        } else {
            format!(" ({reason})")
        };
        return Err(format!(
            "{name} is locked{why}; unlock it with git worktree unlock first"
        ));
    }
    let message = if wt.prunable.is_some() {
        format!("{name}'s folder is already gone.\nDrop it from git's list of worktrees?")
    } else {
        let branch = match &wt.branch {
            Some(b) => format!("Branch {b} stays."),
            None => "Its detached HEAD is only kept in the reflog.".to_string(),
        };
        format!(
            "Remove worktree {name}?\nIts folder is deleted. {branch}\n\
             (If it has uncommitted or untracked changes you will be asked again before they're deleted.)"
        )
    };
    Ok(Modal::confirm(
        "Remove worktree",
        message,
        GitOp::RemoveWorktree {
            path: wt.path.clone(),
            name: name.to_string(),
            branch: wt.branch.clone(),
            force: false,
        },
    ))
}

/// Short state badge and its style.
fn state_text(state: &BranchState, theme: &Theme) -> (String, Style) {
    match state {
        BranchState::LocalOnly => ("local".into(), theme.muted),
        BranchState::RemoteOnly { remotes } => (remotes.join(","), theme.label_remote),
        BranchState::InSync => ("✓".into(), theme.diff_add),
        BranchState::Diverged { ahead, behind } => {
            let mut s = String::new();
            if *ahead > 0 {
                s.push_str(&format!("↑{ahead}"));
            }
            if *behind > 0 {
                s.push_str(&format!("↓{behind}"));
            }
            (s, theme.label_tag)
        }
        BranchState::Gone => ("gone".into(), theme.error),
        BranchState::Untracked { remotes, same_commit } => {
            let sign = if *same_commit { "=" } else { "≠" };
            (format!("{sign}{}", remotes.join(",")), theme.muted)
        }
    }
}

impl Component for BranchesPane {
    fn context(&self) -> Context {
        match self.tab {
            BranchTab::Stash => Context::Stash,
            BranchTab::Worktrees => Context::Worktrees,
            _ => Context::Branches,
        }
    }

    fn on_command(&mut self, cmd: Command, ctx: &Ctx) -> EventState {
        let len = self.visible.len();
        if self.cursor_mut().apply(cmd, len) {
            self.announce(ctx);
            return EventState::Consumed;
        }
        match cmd {
            Command::Filter => self.editing_filter = true,
            Command::NextTab => self.switch_tab(1, ctx),
            Command::PrevTab => self.switch_tab(BranchTab::ALL.len() - 1, ctx),
            Command::PickBase => ctx.emit(Action::OpenModal(self.base_picker())),
            _ if self.tab == BranchTab::Tags => return self.on_tag_command(cmd, ctx),
            _ if self.tab == BranchTab::Stash => return self.on_stash_command(cmd, ctx),
            _ if self.tab == BranchTab::Worktrees => return self.on_worktree_command(cmd, ctx),
            _ => return self.on_entry_command(cmd, ctx),
        }
        EventState::Consumed
    }

    fn commands(&self) -> &[Command] {
        if self.tab == BranchTab::Stash {
            return &[
                Command::StashPop,
                Command::StashApply,
                Command::StashDrop,
                Command::NewBranch,
                Command::ExpandDetails,
                Command::NextTab,
                Command::PrevTab,
                Command::Filter,
            ];
        }
        if self.tab == BranchTab::Tags {
            return &[
                Command::NextTab,
                Command::PrevTab,
                Command::NewBranch,
                Command::NewTag,
                Command::PushTag,
                Command::DeleteBranch,
                Command::Fetch,
                Command::Filter,
                Command::Merge,
                Command::Rebase,
                Command::Copy,
            ];
        }
        if self.tab == BranchTab::Worktrees {
            return &[
                Command::OpenWorktree,
                Command::NewBranch,
                Command::RemoveWorktree,
                Command::Copy,
                Command::NextTab,
                Command::PrevTab,
                Command::Filter,
            ];
        }
        &[
            Command::Checkout,
            Command::NextTab,
            Command::PrevTab,
            Command::PickBase,
            Command::NewBranch,
            Command::NewTag,
            Command::DeleteBranch,
            Command::RenameBranch,
            Command::Fetch,
            Command::Filter,
            Command::Merge,
            Command::Rebase,
            Command::Copy,
        ]
    }

    fn on_click(&mut self, row: usize, ctx: &Ctx) {
        let len = self.visible.len();
        if self.cursor_mut().click(row, len) {
            self.announce(ctx);
        }
    }

    fn on_click_at(&mut self, pos: Position, ctx: &Ctx) -> bool {
        let Some(&(tab, ..)) = self
            .tab_cells
            .iter()
            .find(|(_, y, xs)| *y == pos.y && xs.contains(&pos.x))
        else {
            return false;
        };
        if tab != self.tab {
            self.select_tab(tab, ctx);
        }
        true
    }

    fn wants_text_input(&self) -> bool {
        self.editing_filter
    }

    fn on_text_key(&mut self, key: KeyCombo, ctx: &Ctx) {
        match key.code {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.editing_filter = false;
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Up | KeyCode::Down => {
                let cmd = if key.code == KeyCode::Up {
                    Command::MoveUp
                } else {
                    Command::MoveDown
                };
                let len = self.visible.len();
                self.cursor_mut().apply(cmd, len);
                self.announce(ctx);
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.cursor_mut().selected = 0;
                self.refilter();
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &Theme) {
        // Past the right corner the title is cut off.
        let start = title_x(PaneId::Branches, area);
        let end = area.right().saturating_sub(1);
        let (mut title, mut cells) = self.title(theme, true);
        if start as usize + title.width() > end as usize {
            (title, cells) = self.title(theme, false);
        }
        self.tab_cells = cells
            .into_iter()
            .map(|(tab, xs)| (tab, area.y, (start + xs.start).min(end)..(start + xs.end).min(end)))
            .filter(|(_, _, xs)| !xs.is_empty())
            .collect();
        let mut block = pane_block(PaneId::Branches, title, focused, theme);
        if self.tab == BranchTab::Worktrees {
            let total = format!(" total {} ", self.worktrees_total());
            block = block.title_bottom(Line::from(Span::styled(total, theme.muted)).right_aligned());
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.dates = DrawnDates::new(self.clock.now());
        if self.visible.is_empty() {
            frame.render_widget(Paragraph::new(self.empty_message()).style(theme.muted), inner);
            return;
        }
        let height = inner.height as usize;
        self.cursor_mut().scroll_into_view(height);
        let cursor = *self.cursor();
        let start = cursor.offset.min(self.visible.len());
        let end = (start + height).min(self.visible.len());
        let width = inner.width as usize;
        let list = self.list(self.tab);
        let name_width = self.compared_name_width(&self.visible[start..end], width);
        // Only the tags tab draws distances (and it can list hundreds).
        let distance_width = if matches!(self.tab, BranchTab::Tags) {
            self.tag_distance_width(theme)
        } else {
            0
        };
        let (state_width, size_width) = if self.tab == BranchTab::Worktrees {
            let widest =
                |text: &dyn Fn(&Worktree) -> String| self.worktrees.iter().map(|w| text(w).width()).max().unwrap_or(0);
            (widest(&|w| self.worktree_state(w)), widest(&|w| self.worktree_size(w)))
        } else {
            (0, 0)
        };
        let columns = if self.tab == BranchTab::All {
            self.decoration_columns()
        } else {
            Vec::new()
        };
        let lines: Vec<Line> = (start..end)
            .map(|pos| {
                let i = self.visible[pos];
                let line = match self.tab {
                    BranchTab::All => self.render_entry(&list[i], &columns, width, theme),
                    BranchTab::Tags => self.render_tag(&self.tags[i], name_width, distance_width, width, theme),
                    BranchTab::Stash => self.render_stash(&self.stashes[i], width, theme),
                    BranchTab::Worktrees => self.render_worktree(i, name_width, state_width, size_width, width, theme),
                    _ => self.render_compared(&list[i], name_width, width, theme),
                };
                if pos == cursor.selected {
                    line.patch_style(if focused {
                        theme.selected
                    } else {
                        theme.selected_unfocused
                    })
                } else {
                    line
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        let mut dates = self.dates;
        for &i in &self.visible[start..end] {
            dates.add(match self.tab {
                BranchTab::Tags => self.tags[i].date,
                BranchTab::Stash => self.stashes[i].date,
                BranchTab::Worktrees => continue,
                _ => list[i].activity,
            });
        }
        self.dates = dates;
    }
}
