//! Key parsing and context-scoped key bindings.
//!
//! Bindings live in contexts (lazygit style): a key is looked up in the focused
//! pane's context first and then in [`Context::Global`]. Users override bindings
//! per command in `config.toml`:
//!
//! ```toml
//! [keys.global]
//! quit = ["q", "ctrl+c"]
//!
//! [keys.graph]
//! toggle_scope = "a"
//! ```

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

/// A normalized key press: the code plus ctrl/alt/shift modifiers.
///
/// For character keys the shift modifier is folded into the character itself
/// (`'A'` rather than `shift+a`), because terminals disagree about reporting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let mut modifiers = modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let mut code = code;
        if let KeyCode::Char(c) = code
            && modifiers.contains(KeyModifiers::SHIFT)
        {
            modifiers.remove(KeyModifiers::SHIFT);
            code = KeyCode::Char(c.to_ascii_uppercase());
        }
        // Terminals report shift+tab as BackTab, sometimes with SHIFT set as well.
        if code == KeyCode::BackTab {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Self { code, modifiers }
    }

    pub fn plain(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    pub fn char(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    pub fn to_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

impl From<KeyEvent> for KeyCombo {
    fn from(ev: KeyEvent) -> Self {
        Self::new(ev.code, ev.modifiers)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid key '{0}'")]
pub struct ParseKeyError(pub String);

impl FromStr for KeyCombo {
    type Err = ParseKeyError;

    /// Parses strings like `q`, `Q`, `ctrl+c`, `alt+enter`, `shift+tab`, `f5`, `space`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseKeyError(s.to_string());
        if s.is_empty() {
            return Err(err());
        }
        // A literal "+" (or a combo ending in "++") is the plus key itself.
        let (mods_part, key_part) = if s == "+" {
            ("", "+")
        } else if let Some(prefix) = s.strip_suffix("++") {
            (prefix, "+")
        } else {
            match s.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", s),
            }
        };

        let mut modifiers = KeyModifiers::NONE;
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
                "alt" | "meta" | "m" => KeyModifiers::ALT,
                "shift" | "s" => KeyModifiers::SHIFT,
                _ => return Err(err()),
            };
        }

        let lower = key_part.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => {
                if modifiers.contains(KeyModifiers::SHIFT) {
                    KeyCode::BackTab
                } else {
                    KeyCode::Tab
                }
            }
            "backtab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            _ => {
                let mut chars = key_part.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Char(c),
                    _ => match lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                        Some(n) if (1..=24).contains(&n) => KeyCode::F(n),
                        _ => return Err(err()),
                    },
                }
            }
        };
        Ok(Self::new(code, modifiers))
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            f.write_str("alt+")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            f.write_str("shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::BackTab => f.write_str("shift+tab"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("del"),
            KeyCode::Insert => f.write_str("ins"),
            KeyCode::Up => f.write_str("↑"),
            KeyCode::Down => f.write_str("↓"),
            KeyCode::Left => f.write_str("←"),
            KeyCode::Right => f.write_str("→"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pgup"),
            KeyCode::PageDown => f.write_str("pgdn"),
            KeyCode::F(n) => write!(f, "f{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Where a binding applies. Each pane has its own context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    Global,
    Status,
    Branches,
    Graph,
    Details,
    /// The details pane while it stages a file by hunk.
    Stage,
    /// The branches pane on its Stash tab.
    Stash,
    /// The branches pane on its Worktrees tab.
    Worktrees,
    /// The details pane while it blames a file.
    Blame,
}

/// Everything a key can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Quit,
    Help,
    FocusNext,
    FocusPrev,
    FocusStatus,
    FocusBranches,
    FocusGraph,
    FocusDetails,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    Zoom,
    ToggleDetails,
    ExpandDetails,
    CloseDetails,
    ToggleFiles,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Refresh,
    ToggleScope,
    ToggleDates,
    Checkout,
    NewBranch,
    DeleteBranch,
    RenameBranch,
    NewTag,
    PushTag,
    Fetch,
    Filter,
    PrevTab,
    NextTab,
    PickBase,
    OpenStaging,
    StageToggle,
    StageAll,
    SwitchSide,
    ToggleLineMode,
    SelectRange,
    Discard,
    Commit,
    Amend,
    Push,
    Pull,
    Palette,
    PluginLogs,
    SelfUpdate,
    OpenLink,
    OpenInEditor,
    Copy,
    Merge,
    Rebase,
    CherryPick,
    Stash,
    StashPop,
    StashApply,
    StashDrop,
    Continue,
    Abort,
    Undo,
    OpenWorktree,
    RemoveWorktree,
    Blame,
    OpenCommit,
    BlameBack,
    BlameForward,
    BlameOlder,
    BlameNewer,
    GoToLine,
    Search,
    SearchNext,
    SearchPrev,
    Agent,
}

impl Command {
    /// Short label for the help bar.
    pub fn label(self) -> &'static str {
        match self {
            Command::Quit => "quit",
            Command::Help => "help",
            Command::FocusNext => "next pane",
            Command::FocusPrev => "prev pane",
            Command::FocusStatus => "status",
            Command::FocusBranches => "branches",
            Command::FocusGraph => "graph",
            Command::FocusDetails => "details",
            Command::FocusLeft => "pane left",
            Command::FocusDown => "pane below",
            Command::FocusUp => "pane above",
            Command::FocusRight => "pane right",
            Command::Zoom => "zoom",
            Command::ToggleDetails => "show/hide details",
            Command::ExpandDetails => "files + diff",
            Command::CloseDetails => "close",
            Command::ToggleFiles => "show/hide files",
            Command::MoveUp => "up",
            Command::MoveDown => "down",
            Command::PageUp => "page up",
            Command::PageDown => "page down",
            Command::Top => "top",
            Command::Bottom => "bottom",
            Command::Refresh => "refresh",
            Command::ToggleScope => "all/current",
            Command::ToggleDates => "relative/clock dates",
            Command::Checkout => "checkout",
            Command::NewBranch => "new branch",
            Command::DeleteBranch => "delete",
            Command::RenameBranch => "rename",
            Command::NewTag => "new tag",
            Command::PushTag => "push tag",
            Command::Fetch => "fetch",
            Command::Filter => "filter",
            Command::PrevTab => "prev tab",
            Command::NextTab => "next tab",
            Command::PickBase => "base branch",
            Command::OpenStaging => "stage hunks",
            Command::StageToggle => "stage/unstage",
            Command::SwitchSide => "unstaged/staged",
            Command::ToggleLineMode => "hunks/lines",
            Command::SelectRange => "select lines",
            Command::StageAll => "stage all",
            Command::Discard => "discard",
            Command::Commit => "commit",
            Command::Amend => "amend",
            Command::Push => "push",
            Command::Pull => "pull",
            Command::Palette => "commands",
            Command::PluginLogs => "plugin logs",
            Command::SelfUpdate => "update tt-git",
            Command::OpenLink => "open link",
            Command::OpenInEditor => "edit file",
            Command::Copy => "copy",
            Command::Merge => "merge into current",
            Command::Rebase => "rebase current onto",
            Command::CherryPick => "cherry-pick",
            Command::Continue => "continue merge/rebase",
            Command::Abort => "abort merge/rebase",
            Command::Undo => "undo",
            Command::Stash => "stash",
            Command::StashPop => "pop",
            Command::StashApply => "apply",
            Command::StashDrop => "drop",
            Command::OpenWorktree => "open",
            Command::RemoveWorktree => "remove",
            Command::Blame => "blame",
            Command::OpenCommit => "open commit",
            Command::BlameBack => "step back past commit",
            Command::BlameForward => "forward again",
            Command::BlameOlder => "previous file commit",
            Command::BlameNewer => "next file commit",
            Command::GoToLine => "go to line",
            Command::Search => "search",
            Command::SearchNext => "next match",
            Command::SearchPrev => "prev match",
            Command::Agent => "ask agent",
        }
    }
}

/// Context-scoped key → command table.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Context, HashMap<KeyCombo, Command>>,
    custom: Vec<CustomKey>,
    /// What the help bar and popup show, worked out once from `bindings`
    /// (which never change after building).
    shown: HashMap<(Context, Command), ShownKeys>,
}

/// A command's keys in one context, as [`Keymap::keys_for`] lists them.
#[derive(Debug, Clone)]
struct ShownKeys {
    keys: Vec<KeyCombo>,
    /// The first key, displayed.
    first: String,
    /// All of them displayed, comma-separated.
    all: String,
}

/// One or many key strings, so config can say `quit = "q"` or `quit = ["q", "ctrl+c"]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyList {
    One(String),
    Many(Vec<String>),
}

impl KeyList {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        let slice: &[String] = match self {
            KeyList::One(s) => std::slice::from_ref(s),
            KeyList::Many(v) => v,
        };
        slice.iter().map(String::as_str)
    }
}

/// Per-context command overrides (`[keys.<context>]` tables).
pub type KeyOverrides = HashMap<Context, HashMap<Command, KeyList>>;

/// The `[keys]` table: per-context overrides plus herdr-style custom
/// bindings for plugin actions:
///
/// ```toml
/// [[keys.command]]
/// key = "ctrl+o"
/// type = "plugin_action"
/// command = "acme.github.open-commit"
/// description = "open on GitHub"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct KeysConfig {
    #[serde(default)]
    pub command: Vec<CustomBinding>,
    #[serde(flatten)]
    pub contexts: KeyOverrides,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomKind {
    PluginAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomBinding {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: CustomKind,
    /// Qualified plugin action id.
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Context the key applies in (default: everywhere).
    #[serde(default = "global_context")]
    pub context: Context,
}

fn global_context() -> Context {
    Context::Global
}

/// A resolved custom binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomKey {
    pub context: Context,
    pub key: KeyCombo,
    pub action: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    #[error("keys.{context}.{command}: {source}")]
    BadKey {
        context: String,
        command: String,
        source: ParseKeyError,
    },
    #[error("keys.command[{index}]: {source}")]
    BadCustomKey { index: usize, source: ParseKeyError },
}

const DEFAULTS: &[(Context, Command, &[&str])] = &[
    (Context::Global, Command::Quit, &["q", "ctrl+c"]),
    (Context::Global, Command::Help, &["?"]),
    (Context::Global, Command::FocusNext, &["tab"]),
    (Context::Global, Command::FocusPrev, &["shift+tab"]),
    (Context::Global, Command::FocusGraph, &["1"]),
    (Context::Global, Command::FocusStatus, &["2"]),
    (Context::Global, Command::FocusBranches, &["3"]),
    (Context::Global, Command::FocusDetails, &["4"]),
    (Context::Global, Command::FocusLeft, &["ctrl+h"]),
    (Context::Global, Command::FocusDown, &["ctrl+j"]),
    (Context::Global, Command::FocusUp, &["ctrl+k"]),
    (Context::Global, Command::FocusRight, &["ctrl+l"]),
    (Context::Global, Command::Zoom, &["+"]),
    (Context::Global, Command::MoveUp, &["k", "up"]),
    (Context::Global, Command::MoveDown, &["j", "down"]),
    (Context::Global, Command::PageUp, &["ctrl+u", "pageup"]),
    (Context::Global, Command::PageDown, &["ctrl+d", "pagedown"]),
    (Context::Global, Command::Top, &["g", "home"]),
    (Context::Global, Command::Bottom, &["G", "end"]),
    (Context::Global, Command::Refresh, &["ctrl+r"]),
    (Context::Global, Command::Palette, &[":", "ctrl+p"]),
    (Context::Global, Command::Push, &["P"]),
    (Context::Global, Command::Pull, &["p"]),
    (Context::Global, Command::Fetch, &["ctrl+f"]),
    (Context::Global, Command::Undo, &["z"]),
    (Context::Status, Command::StageToggle, &["space"]),
    (Context::Status, Command::OpenStaging, &["enter"]),
    (Context::Status, Command::StageAll, &["a"]),
    (Context::Status, Command::Discard, &["d"]),
    (Context::Status, Command::Commit, &["c"]),
    (Context::Status, Command::Amend, &["A"]),
    (Context::Status, Command::Stash, &["s"]),
    (Context::Status, Command::OpenInEditor, &["e"]),
    (Context::Status, Command::Blame, &["b"]),
    (Context::Graph, Command::ToggleScope, &["a"]),
    (Context::Graph, Command::ToggleDates, &["t"]),
    (Context::Graph, Command::Checkout, &["o"]),
    (Context::Graph, Command::ExpandDetails, &["enter"]),
    (Context::Graph, Command::ToggleDetails, &["space"]),
    (Context::Graph, Command::CloseDetails, &["esc"]),
    (Context::Graph, Command::NewBranch, &["n"]),
    (Context::Graph, Command::NewTag, &["T"]),
    (Context::Graph, Command::Merge, &["M"]),
    (Context::Graph, Command::Rebase, &["r"]),
    (Context::Graph, Command::CherryPick, &["C"]),
    (Context::Graph, Command::Filter, &["f"]),
    (Context::Graph, Command::Copy, &["c"]),
    (Context::Graph, Command::Agent, &["A"]),
    (Context::Graph, Command::PrevTab, &["[", "{"]),
    (Context::Graph, Command::NextTab, &["]", "}"]),
    (Context::Branches, Command::Copy, &["c"]),
    (Context::Branches, Command::Merge, &["M"]),
    (Context::Branches, Command::Rebase, &["r"]),
    (Context::Status, Command::Continue, &["C"]),
    (Context::Status, Command::Abort, &["X"]),
    (Context::Details, Command::OpenLink, &["o"]),
    (Context::Details, Command::CloseDetails, &["esc", "space"]),
    (Context::Details, Command::ToggleFiles, &["}"]),
    (Context::Details, Command::OpenInEditor, &["e"]),
    (Context::Details, Command::Blame, &["b"]),
    (Context::Blame, Command::GoToLine, &["ctrl+g"]),
    (Context::Blame, Command::OpenCommit, &["enter"]),
    (Context::Blame, Command::ToggleDetails, &["space"]),
    (Context::Blame, Command::BlameBack, &["["]),
    (Context::Blame, Command::BlameForward, &["]"]),
    (Context::Blame, Command::BlameOlder, &["{"]),
    (Context::Blame, Command::BlameNewer, &["}"]),
    (Context::Blame, Command::ToggleDates, &["t"]),
    (Context::Blame, Command::OpenInEditor, &["e"]),
    (Context::Blame, Command::Copy, &["c"]),
    (Context::Blame, Command::CloseDetails, &["esc"]),
    (Context::Blame, Command::Search, &["/"]),
    (Context::Blame, Command::SearchNext, &["n"]),
    (Context::Blame, Command::SearchPrev, &["N"]),
    (Context::Stage, Command::StageToggle, &["space"]),
    (Context::Stage, Command::Discard, &["d"]),
    (Context::Stage, Command::SwitchSide, &["tab"]),
    (Context::Stage, Command::ToggleLineMode, &["a"]),
    (Context::Stage, Command::SelectRange, &["v"]),
    (Context::Stage, Command::CloseDetails, &["esc"]),
    (Context::Stage, Command::Commit, &["c"]),
    (Context::Stage, Command::OpenInEditor, &["e"]),
    (Context::Branches, Command::Checkout, &["enter", "space"]),
    (Context::Branches, Command::NewBranch, &["n"]),
    (Context::Branches, Command::DeleteBranch, &["d"]),
    (Context::Branches, Command::RenameBranch, &["R"]),
    (Context::Branches, Command::NewTag, &["T"]),
    (Context::Branches, Command::PushTag, &["P"]),
    (Context::Branches, Command::Fetch, &["f"]),
    (Context::Branches, Command::Filter, &["/"]),
    (Context::Branches, Command::PrevTab, &["[", "{"]),
    (Context::Branches, Command::NextTab, &["]", "}"]),
    (Context::Branches, Command::PickBase, &["b"]),
    (Context::Stash, Command::StashPop, &["space"]),
    (Context::Stash, Command::StashApply, &["a"]),
    (Context::Stash, Command::StashDrop, &["d"]),
    (Context::Stash, Command::NewBranch, &["n"]),
    (Context::Stash, Command::ExpandDetails, &["enter"]),
    (Context::Stash, Command::NextTab, &["]", "}"]),
    (Context::Stash, Command::PrevTab, &["[", "{"]),
    (Context::Stash, Command::Filter, &["/"]),
    (Context::Worktrees, Command::OpenWorktree, &["enter", "space"]),
    (Context::Worktrees, Command::RemoveWorktree, &["d"]),
    (Context::Worktrees, Command::NewBranch, &["n"]),
    (Context::Worktrees, Command::Copy, &["c"]),
    (Context::Worktrees, Command::NextTab, &["]", "}"]),
    (Context::Worktrees, Command::PrevTab, &["[", "{"]),
    (Context::Worktrees, Command::Filter, &["/"]),
];

impl Default for Keymap {
    fn default() -> Self {
        Self::with_overrides(&KeyOverrides::default()).expect("default keymap is valid")
    }
}

impl Keymap {
    /// Builds the default keymap, then replaces the bindings of every command
    /// the user listed (per context).
    pub fn with_overrides(overrides: &KeyOverrides) -> Result<Self, KeymapError> {
        let mut table: HashMap<(Context, Command), Vec<KeyCombo>> = HashMap::new();
        for (ctx, cmd, keys) in DEFAULTS {
            let combos = keys.iter().map(|k| k.parse().expect("default key parses")).collect();
            table.insert((*ctx, *cmd), combos);
        }
        for (ctx, cmds) in overrides {
            for (cmd, keys) in cmds {
                let combos = keys
                    .iter()
                    .map(|k| {
                        k.parse().map_err(|source| KeymapError::BadKey {
                            context: serde_name(ctx),
                            command: serde_name(cmd),
                            source,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                table.insert((*ctx, *cmd), combos);
            }
        }

        let mut bindings: HashMap<Context, HashMap<KeyCombo, Command>> = HashMap::new();
        // Sort so the result does not depend on HashMap iteration order when
        // two commands claim the same key.
        let mut entries: Vec<_> = table.into_iter().collect();
        entries.sort_by_key(|((ctx, cmd), _)| (serde_name(ctx), serde_name(cmd)));
        for ((ctx, cmd), combos) in entries {
            let map = bindings.entry(ctx).or_default();
            for combo in combos {
                map.entry(combo).or_insert(cmd);
            }
        }
        let mut keys: HashMap<(Context, Command), Vec<KeyCombo>> = HashMap::new();
        for (ctx, map) in &bindings {
            for (key, cmd) in map {
                keys.entry((*ctx, *cmd)).or_default().push(*key);
            }
        }
        let shown = keys
            .into_iter()
            .map(|(at, mut keys)| {
                keys.sort_by_cached_key(|k| {
                    let s = k.to_string();
                    (k.modifiers.bits(), s.chars().count() > 1, s)
                });
                let names: Vec<String> = keys.iter().map(ToString::to_string).collect();
                let shown = ShownKeys {
                    first: names[0].clone(),
                    all: names.join(", "),
                    keys,
                };
                (at, shown)
            })
            .collect();
        Ok(Self {
            bindings,
            custom: Vec::new(),
            shown,
        })
    }

    /// Builds from the full `[keys]` table, including custom bindings.
    pub fn from_config(keys: &KeysConfig) -> Result<Self, KeymapError> {
        let mut km = Self::with_overrides(&keys.contexts)?;
        for (index, b) in keys.command.iter().enumerate() {
            let key = b
                .key
                .parse()
                .map_err(|source| KeymapError::BadCustomKey { index, source })?;
            km.custom.push(CustomKey {
                context: b.context,
                key,
                action: b.command.clone(),
                description: b.description.clone(),
            });
        }
        Ok(km)
    }

    /// A custom (plugin action) binding for `key`, preferring `context`
    /// over global ones. Checked before the built-in bindings.
    pub fn lookup_custom(&self, context: Context, key: KeyCombo) -> Option<&CustomKey> {
        self.custom
            .iter()
            .find(|c| c.key == key && c.context == context)
            .or_else(|| {
                self.custom
                    .iter()
                    .find(|c| c.key == key && c.context == Context::Global)
            })
    }

    pub fn custom(&self) -> &[CustomKey] {
        &self.custom
    }

    /// Looks up a key in `context`, falling back to the global context.
    pub fn lookup(&self, context: Context, key: KeyCombo) -> Option<Command> {
        self.bindings
            .get(&context)
            .and_then(|m| m.get(&key))
            .or_else(|| self.bindings.get(&Context::Global).and_then(|m| m.get(&key)))
            .copied()
    }

    /// All keys bound to `command` in `context`, sorted for stable display.
    pub fn keys_for(&self, context: Context, command: Command) -> Vec<KeyCombo> {
        self.shown
            .get(&(context, command))
            .map(|s| s.keys.clone())
            .unwrap_or_default()
    }

    /// [`keys_for`](Self::keys_for), displayed and comma-separated; empty
    /// when there are none.
    pub fn keys_text(&self, context: Context, command: Command) -> &str {
        self.shown.get(&(context, command)).map_or("", |s| s.all.as_str())
    }

    /// The key shown in the help bar for `command`: the first binding in
    /// `context`, else in the global context.
    pub fn display_key(&self, context: Context, command: Command) -> Option<KeyCombo> {
        self.shown_keys(context, command).map(|s| s.keys[0])
    }

    /// [`display_key`](Self::display_key), displayed.
    pub fn display_key_text(&self, context: Context, command: Command) -> Option<&str> {
        self.shown_keys(context, command).map(|s| s.first.as_str())
    }

    fn shown_keys(&self, context: Context, command: Command) -> Option<&ShownKeys> {
        self.shown
            .get(&(context, command))
            .or_else(|| self.shown.get(&(Context::Global, command)))
    }
}

/// The snake_case config name of a unit enum variant.
fn serde_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> KeyCombo {
        s.parse().unwrap()
    }

    #[test]
    fn parses_keys() {
        assert_eq!(k("q"), KeyCombo::char('q'));
        assert_eq!(k("Q"), KeyCombo::char('Q'));
        assert_eq!(k("shift+q"), KeyCombo::char('Q'));
        assert_eq!(k("ctrl+c"), KeyCombo::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            k("ctrl+alt+x"),
            KeyCombo::new(KeyCode::Char('x'), KeyModifiers::CONTROL | KeyModifiers::ALT)
        );
        assert_eq!(k("shift+tab"), KeyCombo::plain(KeyCode::BackTab));
        assert_eq!(k("enter"), KeyCombo::plain(KeyCode::Enter));
        assert_eq!(k("space"), KeyCombo::char(' '));
        assert_eq!(k("f5"), KeyCombo::plain(KeyCode::F(5)));
        assert_eq!(k("+"), KeyCombo::char('+'));
        assert_eq!(k("ctrl++"), KeyCombo::new(KeyCode::Char('+'), KeyModifiers::CONTROL));
        assert!("ctrl+".parse::<KeyCombo>().is_err());
        assert!("hyper+x".parse::<KeyCombo>().is_err());
        assert!("f99".parse::<KeyCombo>().is_err());
        assert!("".parse::<KeyCombo>().is_err());
    }

    #[test]
    fn normalizes_terminal_events() {
        // crossterm reports an uppercase letter with SHIFT set.
        let ev = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(KeyCombo::from(ev), KeyCombo::char('G'));
        let ev = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(KeyCombo::from(ev), KeyCombo::plain(KeyCode::BackTab));
    }

    #[test]
    fn display_round_trips() {
        for s in ["q", "ctrl+c", "alt+enter", "f5", "space", "shift+tab"] {
            let combo = k(s);
            let shown = combo.to_string();
            if !shown.contains('↑') {
                assert_eq!(shown.parse::<KeyCombo>().unwrap(), combo, "{s}");
            }
        }
    }

    #[test]
    fn context_lookup_falls_back_to_global() {
        let km = Keymap::default();
        assert_eq!(km.lookup(Context::Graph, k("a")), Some(Command::ToggleScope));
        assert_eq!(km.lookup(Context::Branches, k("a")), None);
        assert_eq!(km.lookup(Context::Graph, k("q")), Some(Command::Quit));
        assert_eq!(km.lookup(Context::Status, k("j")), Some(Command::MoveDown));
    }

    #[test]
    fn overrides_replace_command_bindings() {
        let overrides: KeyOverrides = toml::from_str::<HashMap<Context, HashMap<Command, KeyList>>>(
            r#"
            [global]
            quit = "x"
            [graph]
            toggle_scope = ["b", "ctrl+a"]
            "#,
        )
        .unwrap();
        let km = Keymap::with_overrides(&overrides).unwrap();
        assert_eq!(km.lookup(Context::Global, k("x")), Some(Command::Quit));
        assert_eq!(km.lookup(Context::Global, k("q")), None);
        assert_eq!(km.lookup(Context::Graph, k("ctrl+a")), Some(Command::ToggleScope));
        assert_eq!(km.lookup(Context::Graph, k("a")), None);
        assert_eq!(km.display_key(Context::Graph, Command::ToggleScope), Some(k("b")));
    }

    #[test]
    fn custom_plugin_bindings() {
        let keys: KeysConfig = toml::from_str(
            r#"
            [[command]]
            key = "ctrl+o"
            type = "plugin_action"
            command = "acme.gh.open"
            description = "open on GitHub"

            [[command]]
            key = "o"
            type = "plugin_action"
            command = "acme.gh.branch"
            context = "branches"

            [graph]
            toggle_scope = "x"
            "#,
        )
        .unwrap();
        let km = Keymap::from_config(&keys).unwrap();
        assert_eq!(
            km.lookup_custom(Context::Graph, k("ctrl+o")).unwrap().action,
            "acme.gh.open"
        );
        assert_eq!(
            km.lookup_custom(Context::Branches, k("o")).unwrap().action,
            "acme.gh.branch"
        );
        assert!(km.lookup_custom(Context::Graph, k("o")).is_none());
        assert_eq!(km.lookup(Context::Graph, k("x")), Some(Command::ToggleScope));

        let bad: KeysConfig =
            toml::from_str("[[command]]\nkey = \"nope+x\"\ntype = \"plugin_action\"\ncommand = \"a.b\"\n").unwrap();
        assert_eq!(
            Keymap::from_config(&bad).unwrap_err().to_string(),
            "keys.command[0]: invalid key 'nope+x'"
        );
    }

    #[test]
    fn bad_override_reports_location() {
        let mut overrides = KeyOverrides::default();
        overrides
            .entry(Context::Graph)
            .or_default()
            .insert(Command::ToggleScope, KeyList::One("hyper+x".into()));
        let err = Keymap::with_overrides(&overrides).unwrap_err();
        assert_eq!(err.to_string(), "keys.graph.toggle_scope: invalid key 'hyper+x'");
    }

    /// What `keys_for` and `display_key` worked out on every call before
    /// they were cached.
    fn uncached_keys_for(km: &Keymap, context: Context, command: Command) -> Vec<KeyCombo> {
        let mut keys: Vec<KeyCombo> = km
            .bindings
            .get(&context)
            .into_iter()
            .flat_map(|m| m.iter())
            .filter(|(_, c)| **c == command)
            .map(|(k, _)| *k)
            .collect();
        keys.sort_by_key(|k| (k.modifiers.bits(), k.to_string().chars().count() > 1, k.to_string()));
        keys
    }

    #[test]
    fn cached_key_display_matches_working_it_out() {
        let overrides: KeyOverrides = toml::from_str::<HashMap<Context, HashMap<Command, KeyList>>>(
            r#"
            [global]
            quit = ["x", "ctrl+q", "f10", "Q", "esc"]
            help = "x"
            [graph]
            toggle_scope = ["b", "ctrl+a", "alt+shift+b", "enter", "B"]
            [status]
            quit = "z"
            "#,
        )
        .unwrap();
        let contexts = [
            Context::Global,
            Context::Status,
            Context::Branches,
            Context::Graph,
            Context::Details,
            Context::Stage,
            Context::Stash,
            Context::Worktrees,
            Context::Blame,
        ];
        let commands: std::collections::HashSet<Command> = DEFAULTS.iter().map(|(_, cmd, _)| *cmd).collect();
        for km in [Keymap::default(), Keymap::with_overrides(&overrides).unwrap()] {
            for ctx in contexts {
                for cmd in &commands {
                    let want = uncached_keys_for(&km, ctx, *cmd);
                    assert_eq!(km.keys_for(ctx, *cmd), want, "{ctx:?} {cmd:?}");
                    let text = want.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                    assert_eq!(km.keys_text(ctx, *cmd), text);
                    let display = want
                        .first()
                        .copied()
                        .or_else(|| uncached_keys_for(&km, Context::Global, *cmd).first().copied());
                    assert_eq!(km.display_key(ctx, *cmd), display, "{ctx:?} {cmd:?}");
                    assert_eq!(
                        km.display_key_text(ctx, *cmd).map(str::to_string),
                        display.map(|k| k.to_string())
                    );
                }
            }
        }
    }
}
