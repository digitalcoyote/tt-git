<p align="center">
  <img src="docs/assets/logo.svg" alt="" width="128" height="128" />
</p>

<h1 align="center">tt-git</h1>

![tt-git on a web project: the commit graph on top, the status pane and the branch list below](docs-site/src/assets/screens/hero.webp)

A terminal git client in Rust ([ratatui](https://ratatui.rs)), inspired by
[lazygit](https://github.com/jesseduffield/lazygit) and
[gitui](https://github.com/gitui-org/gitui), with:

- **A GitLens-style commit graph as the main view.** It shows `git log --all`
  with stable, colored lanes and badges for branches, remotes and tags.
  Press `a` to switch between all branches and just the current branch (plus
  its upstream).
- **A status pane** like lazygit's files panel: stage, unstage, discard,
  commit, amend and stash. Press `enter` on a file to see its changes
  fullscreen and stage, unstage or discard them a hunk or a line at a time.
  It also handles merge/rebase/cherry-pick conflicts.
- **Code colored by language** in diffs, the staging view and blame, with
  added and removed lines on a green or red background. Around 200 languages,
  from bat's syntax definitions.
- **One branch list for local and remote branches**, sorted by recent
  activity (commits, pushes, checkouts). If you check out a branch that only
  exists on a remote, tt-git creates the local tracking branch for you
  without asking.
  Two more tabs list the branches merged and not yet merged into the
  project's primary branch (or any base branch you pick). On those tabs the
  graph shows only the selected branch, from where it branched off to where
  it was merged back in.
- **Plugins built the same way as [herdr](https://herdr.dev/docs/plugins/):**
  plain subprocesses with a TOML manifest. Plugins can add actions, event
  hooks, startup commands and terminal panes, and they talk back through the
  `tt-git` CLI or a JSON socket. See [docs/plugins.md](docs/plugins.md).

```
╭[1] Graph [all] 1/5─────────────────────────────────────────────────────────────────────────────╮
│●    [side] side experiment                                             Test User      2h b66   │
│◉─╮   main  <v1.0> Merge 'feature'                                      Test User      3h 59f   │
││ ●  [feature] feature work                                             Test User      1d 788   │
│● │  main work                                                          Test User      1d f75   │
│●─╯  base                                                               Test User      2d b29   │
╰────────────────────────────────────────────────────────────────────────────────────────────────╯
╭[2] Status: repo · main → origin/main ↑1───────╮╭[3] Branches 3─────────────────────────────────╮
│ M src/app.rs                                  ││* main            ✓          2h                │
│?? notes.md                                    ││  feature         local      1d                │
╰───────────────────────────────────────────────╯╰───────────────────────────────────────────────╯
 enter files + diff  space show/hide details  a all/current  n new branch  tab next pane  ? help
```

Press `space` on a commit to open its details in place of the bottom row,
or `enter` to open it fullscreen as changed files beside a diff.
The layout is configurable; see [docs/configuration.md](docs/configuration.md#layout).

## Install

You need git 2.31 or newer on `PATH`.

Linux (x64, arm64) and macOS (Apple Silicon):

```sh
curl -fsSL https://github.com/ThirtyTech/tt-git/releases/latest/download/install.sh | sh
```

Windows (x64, arm64):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/ThirtyTech/tt-git/releases/latest/download/install.ps1 | iex"
```

Both install the latest release; set `TTGIT_VERSION` (such as `v0.2.0`) for
a specific one, or `TTGIT_INSTALL_DIR` to change where it goes. You can also
download an archive from
[Releases](https://github.com/ThirtyTech/tt-git/releases).

To update later, run `tt-git update` (or *Update tt-git* in the command
palette). `tt-git update --check` only checks.

To build from source:

```sh
cargo install --path crates/tt-git
tt-git            # in a repository, or: tt-git path/to/repo
```

## Keys

Press `?` for the keys of the focused pane (type to search them, `esc` to
close), or `:` (or `ctrl+p`) for the
command palette, which lists built-in commands and plugin actions.

| Where | Key | Does |
|---|---|---|
| anywhere | `1` `2` `3` `4` / `tab` `shift+tab` | focus graph / status / branches / details |
| | `ctrl+h` `ctrl+j` `ctrl+k` `ctrl+l` | focus the pane to the left / below / above / right |
| | `+` | zoom: the graph, or details if focused, fullscreen and back |
| | `j` `k` `↑` `↓`, `ctrl+d` `ctrl+u`, `g` `G` | move, page, top/bottom |
| | `P` / `p` | push (sets upstream on first push) / pull |
| | `ctrl+f` | `fetch --all --prune` |
| | `z` | undo the last checkout, commit, reset or rebase |
| | `ctrl+r` | refresh |
| | `:` `ctrl+p` | command palette |
| | `q` | quit |
| graph | `enter` | the commit fullscreen: changed files on the left, the selected file's diff on the right (`j`/`k` pick a file, `pgup`/`pgdn` scroll, `esc` or `space` back) |
| | `space` | show / hide the commit's details (they replace the bottom row) |
| | `esc` | hide details (with details hidden: clear the filter) |
| | `a` | all branches ↔ current branch |
| | `t` | dates as age (`3h`) ↔ local date and time (`2026-10-17 01:30 PM`) |
| | `f` | filter: only one author's commits, or the commits that changed a file or folder (`tab` in the panel switches between the two; both can be set) |
| | `o` | check out the selected commit: the local branch on it if there is one, else the commit with HEAD detached (with uncommitted changes, asks to stash them first) |
| | `n` | new branch at the selected commit (`tab` in the dialog: create it in a worktree) |
| | `T` | new tag at the selected commit (`tab` in the dialog: annotated, with a message) |
| | `M` / `r` / `C` | merge into current / rebase current onto / cherry-pick |
| | `c` | copy the commit hash |
| | `A` | ask an AI agent about the commit, or start a ready-made action (explain branch, resolve conflicts…); opens in a herdr tab inside herdr, else in place of details |
| | `]` `[` (or `}` `{`) | next / previous tab in the branches pane, without leaving the graph |
| branches | `enter` `space` | check out (remote-only branches get a tracking branch) |
| | `n` `d` `R` | new (`tab` in the dialog: create it in a worktree under `.worktrees/`) / delete (asks before force-deleting unmerged) / rename |
| | `T` | new tag at the selected branch or tag (`tab` in the dialog: annotated) |
| | `P` | on the tags tab: push the selected tag (elsewhere: push the current branch) |
| | `d` (tags tab) | delete the tag: `y` here only, `r` here and on the remote |
| | `f` | `fetch --all --prune` |
| | `/` | filter as you type (`esc` clears) |
| | `]` `[` | next / previous tab: all branches, tags, unmerged into the base branch, merged into it, stashes and worktrees (once there are any) |
| | `b` | pick the base branch (default: the project's primary branch) |
| | `M` / `r` | merge / rebase onto the selected branch |
| | `c` | copy the branch name |
| status | `space` | stage / unstage the file |
| | `enter` | open the file's changes fullscreen to stage them by hunk or line (below) |
| | `a` | stage everything (or unstage, if all staged) |
| | `d` | discard changes to the file (asks first) |
| | `c` / `A` | commit / amend keeping the message |
| | `s` | stash: everything, only what's staged, only what isn't staged, everything but leave staged changes in place, or just the selected file (`tab` in the dialog picks; the message is optional) |
| | `e` | open the file in your editor at its first change (a conflicted file: at its first conflict marker) |
| | `b` | blame the file in the working tree: who last changed each line, when and in which commit |
| | `C` / `X` | continue / abort a merge, rebase or cherry-pick |
| stash tab | `space` / `a` | pop / apply the selected stash (pop drops it once applied) |
| | `d` | drop it (asks first) |
| | `n` | new branch from it at the commit it was made on (`git stash branch`) |
| | `enter` | show what it holds fullscreen, untracked files included (`esc` back) |
| worktrees tab | `enter` `space` | open the selected worktree in place of this one (tt-git starts over there) |
| | `d` | remove it (asks first; asks again before `--force` if it has changes; never the main worktree or the one tt-git is in) |
| | `n` / `c` | new branch in a new worktree from it / copy its absolute path |
| details | `}` | show / hide the commit's changed files below the diff (`j`/`k` pick a file); fullscreen puts them on the left |
| | `o` | open the link in view with a plugin link handler |
| | `e` | open the file in view in your editor, at the line at the top of the diff |
| | `b` | blame the file in view as of that commit, at the line at the top of the diff |
| | `esc` `space` | close details |
| staging | `j` / `k` | next / previous hunk (or changed line) |
| | `space` | stage the selection, or unstage it on the staged side |
| | `tab` | switch between the unstaged and staged changes |
| | `a` | pick single lines instead of whole hunks, and back |
| | `v` | start or end a range of lines |
| | `d` | discard the selection from the working tree (asks first) |
| | `c` | commit what's staged |
| | `e` | open the file in your editor at the selected hunk or line |
| | `esc` | back to the status pane |
| blame | `ctrl+g` | go to a line by number (vim style works too: `42G`, `5j`, `:42` `enter`) |
| | `enter` | open the line's commit fullscreen, scrolled to the line (`b` there blames another of its files; `esc` back) |
| | `space` | show / hide the line's commit: author, date, how many lines it gave this file, full message |
| | `[` / `]` | step back to the file as it was before the line's commit / forward again |
| | `{` / `}` | step back / forward one commit of the file at a time |
| | `/` | search the code with a regex, jumping as you type (`enter` keeps it, `esc` drops it) |
| | `n` / `N` | next / previous matching line |
| | `t` | ages ↔ local dates and times (shared with the graph) |
| | `e` / `c` | open the file in your editor at the line / copy the line's commit hash |
| | `esc` | back to where blame was opened from |

The mouse also works: click a pane or row to focus and select it,
double-click a commit to open its files fullscreen, click a tab in the
branches pane's title, a file in the changed-files view or an entry
in the graph's filter popup, use the wheel to scroll the pane under the
pointer, and `ctrl`+click a link in the details pane. Every binding can be changed; see
[docs/configuration.md](docs/configuration.md).

## Documentation

The full documentation is at <https://thirtytech.github.io/tt-git/>. For how each part works:

- [Graph](https://thirtytech.github.io/tt-git/panes/graph/): lanes, badges, all or current, loading,
  filtering by author or file
- [Details](https://thirtytech.github.io/tt-git/panes/details/): the commit, its changed files, links
- [Branches](https://thirtytech.github.io/tt-git/panes/branches/): one row per branch, sorting by activity,
  the status column
- [Merged and unmerged](https://thirtytech.github.io/tt-git/panes/merged-unmerged/): the base branch and the
  branch history in the graph
- [Tags](https://thirtytech.github.io/tt-git/panes/tags/) and [Stash](https://thirtytech.github.io/tt-git/panes/stash/)
- [Status](https://thirtytech.github.io/tt-git/panes/status/) and [staging hunks and lines](https://thirtytech.github.io/tt-git/panes/staging/)
- [Plugin panes](https://thirtytech.github.io/tt-git/panes/plugin-panes/)
- [Undo](https://thirtytech.github.io/tt-git/git/undo/): stepping back through checkouts, commits,
  resets and rebases
- [Refreshing](https://thirtytech.github.io/tt-git/git/refreshing/): what tt-git watches and when it reloads
- [How it works](https://thirtytech.github.io/tt-git/reference/how-it-works/): the git commands behind each
  pane
- [Configuration](https://thirtytech.github.io/tt-git/configuration/): settings, keys and layout

## Memory and speed

tt-git is built to sit open all day next to your editor without costing
anything while you aren't using it:

- It redraws only when something changed. The 250 ms tick redraws only when
  a relative date on screen would change, plugins reloaded, or a plugin
  pane shows a cursor.
- It runs on a single-threaded async runtime. git work runs on a blocking
  thread pool whose threads exit once they've been idle a few seconds, so
  an idle tt-git has a handful of threads.
- Each kind of load keeps at most one git process in flight, with only the
  newest request waiting. Holding `j` through the graph doesn't start a
  `git show` per row, and a graph load that's been replaced is killed
  rather than finished.
- Commits are compact: hashes are 32-byte arrays rather than strings,
  author names and emails are shared between commits, and the graph stores
  only the lanes it draws, at 2 bytes a lane.
- A refs refresh is 6 to 8 git processes however many branches and tags
  there are; reflogs and the ref fingerprint are read from files directly.

### An example: a large React codebase

Measured on a long-lived React and TypeScript web app:

| | |
|---|---|
| History | 28,145 commits on all refs (27,281 on `HEAD`), since 2017, 42 authors |
| Refs | 810: 160 local branches, 303 remote branches, 341 tags |
| Tracked files | 3,209 (1,111 `.tsx`/`.jsx`, 1,637 `.ts`/`.js`) |
| Packed objects | 246k objects, 103 MiB |
| Working tree | with an installed `node_modules/` (ignored) |

tt-git release build, Windows 11, git 2.50, `--no-plugins`, default config
(so the graph loads the newest 20,000 commits), sampled every second for
45 seconds after starting, with no keys pressed:

| | |
|---|---|
| Peak working set | 30.5 MiB |
| Working set, idle | 28.2 to 29.2 MiB |
| Private bytes, idle | 17.9 to 18.9 MiB |
| Threads | 11 while loading, 5 at idle |
| CPU time | 0.17 s to load, 0.31 s total over 45 s |
| git processes at idle | none |

Refs, status and the first 20,000 commits of the graph had loaded by the
first sample, one second in; the extra threads wind down over the next
20 seconds. The binary is 6.4 MiB.

## Plugins

See [Plugins](https://thirtytech.github.io/tt-git/plugins/) on the doc site.

```sh
tt-git plugin install owner/repo          # from GitHub; shows what will run and asks first
tt-git plugin link ./my-plugin            # local development
tt-git plugin list
```

[docs/plugins.md](docs/plugins.md) covers the manifest format, environment
variables, events, panes, the socket API and the CLI.
[examples/plugins](examples/plugins) has working examples.

The [Azure DevOps plugin](plugins/azure-devops) shows each branch's active
pull request in the branch list:

```sh
tt-git plugin install ThirtyTech/tt-git/plugins/azure-devops
```

## Development

You need Rust 1.89 or newer (the workspace uses edition 2024) and git 2.31
or newer on `PATH`.

### Running from source

```sh
cargo run -p tt-git                        # the repository in the current directory
cargo run -p tt-git -- path/to/repo        # another repository
cargo run -p tt-git -- --no-plugins        # without plugins or the API socket
cargo run -p tt-git -- --config dev.toml   # with a different config file
cargo run -p tt-git -- plugin list         # subcommands work the same way
```

Everything after `--` goes to tt-git rather than Cargo.

A dev build reads your real config and installed plugins. To keep it
separate, point the config and state directories somewhere under `target/`:

```sh
export TTGIT_CONFIG_DIR=$PWD/target/dev/config
export TTGIT_STATE_DIR=$PWD/target/dev/state
```

```powershell
$env:TTGIT_CONFIG_DIR = "$PWD\target\dev\config"
$env:TTGIT_STATE_DIR = "$PWD\target\dev\state"
```

### Logs

The UI owns the terminal, so tt-git logs to `<state>/logs/tt-git.log`
instead (see [docs/configuration.md](docs/configuration.md#logs) for where
`<state>` is). `TTGIT_LOG` sets the level as an
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
string, default `info`. Follow the log from a second terminal:

```sh
TTGIT_LOG=debug,tt_git_tui=trace cargo run -p tt-git
tail -f target/dev/state/logs/tt-git.log   # PowerShell: Get-Content target\dev\state\logs\tt-git.log -Wait
```

### Working on plugins

`plugin link` uses a plugin straight from its directory, so edits take
effect without reinstalling:

```sh
cargo run -p tt-git -- plugin link examples/plugins/open-on-github
```

[docs/plugins.md](docs/plugins.md#testing-a-plugin) covers testing plugins.

### Tests

```sh
cargo test --workspace            # unit, integration, in-process UI and PTY end-to-end tests
cargo insta review                # after intentional UI changes
cargo bench -p tt-git-graph       # graph layout on a synthetic 100k-commit history
```

Tests run against real git in throwaway repositories with a fake `$HOME`
and fixed dates (`Sandbox`). UI flows drive the real app on ratatui's
`TestBackend` and wait until the app is idle after each key (`TestDriver`),
so they never sleep. A few end-to-end tests run the real binary in a
pseudo-terminal, ConPTY on Windows, via [termlens](https://crates.io/crates/termlens).

### Before you push

CI checks formatting and lints, then runs the tests on Linux, macOS and
Windows with [cargo-nextest](https://nexte.st) and
[cargo-insta](https://insta.rs). To run the same checks locally:

```sh
cargo install cargo-nextest cargo-insta   # once
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo insta test --workspace --test-runner nextest --check --unreferenced reject
cargo test --workspace --doc
```

`--check` fails on any snapshot that doesn't match, and
`--unreferenced reject` fails on snapshot files no test uses, so delete
those when you remove a test.

### Crates

| Crate | What |
|---|---|
| `tt-git-core` | git CLI runner, parsers, unified branch list, write operations |
| `tt-git-graph` | lane layout for the commit graph (no git or UI dependencies) |
| `tt-git-plugin` | manifest, registry, runtime, install/GC, NDJSON socket |
| `tt-git-tui` | the ratatui app: panes, keymap, modals, PTY plugin panes |
| `tt-git` | the binary and CLI |
| `tt-git-testkit` | sandbox repos, the in-process `TestDriver`, plugin fixtures |
