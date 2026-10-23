//! The help bar while a pull, push or fetch runs, and once it's done.
//! `press_frame` and `tick` keep the git job unreported, so the app still
//! sees it running.

use ratatui::style::Color;
use tt_git_testkit::{Sandbox, TestDriver};
use tt_git_tui::Action;
use tt_git_tui::modal::{GitOp, Modal};

/// `main` tracking `origin/main`, one commit ahead.
fn ahead_of_origin() -> Sandbox {
    let sb = Sandbox::new();
    sb.commit_file("a.txt", "1", "base");
    sb.add_remote("origin");
    sb.git(&["push", "-q", "-u", "origin", "HEAD"]);
    sb.commit_file("a.txt", "2", "local work");
    sb
}

fn help_bar(t: &TestDriver) -> String {
    t.screen().lines().last().unwrap_or_default().to_string()
}

/// The foreground color of the first cell of `text` in the help bar.
fn color_of(t: &TestDriver, text: &str) -> Option<Color> {
    let (x, y) = t.find_text(text)?;
    Some(t.buffer()[(x, y)].fg)
}

#[test]
fn a_running_push_shows_a_spinner_and_how_long_it_has_run() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press_frame("P");
    insta::assert_snapshot!(help_bar(&t));
    assert_eq!(color_of(&t, "⠋ pushing"), Some(Color::Cyan));

    // A second in, the spinner has turned and the time shows.
    t.tick(4);
    assert!(
        help_bar(&t).ends_with("│ ⠼ pushing main → origin/main  1s"),
        "{}",
        help_bar(&t)
    );
    t.tick(8);
    assert!(
        help_bar(&t).ends_with("│ ⠹ pushing main → origin/main  3s"),
        "{}",
        help_bar(&t)
    );

    t.wait_idle();
    assert!(t.app().error().is_none(), "{:?}", t.app().error());
    assert_eq!(t.app().notice(), Some("✓ pushed main → origin/main"));
    assert!(
        help_bar(&t).ends_with("│ ✓ pushed main → origin/main"),
        "{}",
        help_bar(&t)
    );
}

#[test]
fn a_finished_push_is_green_then_fades() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press("P");
    assert_eq!(color_of(&t, "✓ pushed"), Some(Color::Green));
    t.tick(19);
    assert_eq!(color_of(&t, "✓ pushed"), Some(Color::Green));
    t.tick(1);
    assert_eq!(color_of(&t, "✓ pushed"), Some(Color::DarkGray));

    // The same words again are a new result: green again.
    sb.commit_file("a.txt", "3", "more work");
    t.press("P");
    assert_eq!(t.app().notice(), Some("✓ pushed main → origin/main"));
    assert_eq!(color_of(&t, "✓ pushed"), Some(Color::Green));
}

#[test]
fn ticks_redraw_while_the_spinner_turns_and_the_tick_fades() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press_frame("P");
    t.app_mut().take_redraw();
    t.tick(1);
    assert!(t.app_mut().take_redraw(), "spinner");

    t.wait_idle();
    t.app_mut().take_redraw();
    t.tick(1);
    assert!(t.app_mut().take_redraw(), "green ✓");

    t.tick(20);
    t.app_mut().take_redraw();
    t.tick(1);
    assert!(!t.app_mut().take_redraw(), "faded");
}

#[test]
fn pushing_again_while_a_push_runs_says_so_and_starts_nothing() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press_frame("P").tick(2).press_frame("P");
    assert_eq!(t.app().notice(), Some("already pushing"));
    assert!(
        help_bar(&t).ends_with("│ already pushing · ⠹ pushing main → origin/main"),
        "{}",
        help_bar(&t)
    );

    // After a moment only the push is left.
    t.tick(12);
    assert!(
        help_bar(&t).ends_with("│ ⠼ pushing main → origin/main  3s"),
        "{}",
        help_bar(&t)
    );

    // One push ran: one git push in the log.
    t.wait_idle();
    let pushes = t
        .app()
        .repo()
        .git()
        .log()
        .entries()
        .iter()
        .filter(|c| c.args.starts_with("push"))
        .count();
    assert_eq!(pushes, 1);
    assert_eq!(t.app().notice(), Some("✓ pushed main → origin/main"));
}

#[test]
fn a_notice_shows_beside_a_running_push_for_a_moment() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press_frame("P");
    t.send_frame(Action::Notify("copied 4b15452".into())).tick(1);
    assert!(
        help_bar(&t).ends_with("│ copied 4b15452 · ⠙ pushing main → origin/main"),
        "{}",
        help_bar(&t)
    );
    t.tick(12);
    assert!(
        help_bar(&t).ends_with("│ ⠸ pushing main → origin/main  3s"),
        "{}",
        help_bar(&t)
    );
}

#[test]
fn pull_and_fetch_say_where_they_go_and_several_show_their_verbs() {
    let sb = ahead_of_origin();
    let mut t = TestDriver::new(&sb);
    t.press_frame("p");
    assert!(
        help_bar(&t).ends_with("│ ⠋ pulling origin/main into main"),
        "{}",
        help_bar(&t)
    );
    t.wait_idle();
    assert_eq!(t.app().notice(), Some("✓ pulled origin/main into main"));

    // The branches pane's `f` queues this; sent here so it stays running.
    t.send_frame(Action::Run(GitOp::FetchAll));
    assert!(help_bar(&t).ends_with("│ ⠋ fetching all remotes"), "{}", help_bar(&t));
    t.press_frame("P");
    assert!(help_bar(&t).ends_with("│ ⠋ fetching · pushing"), "{}", help_bar(&t));
    t.wait_idle();
    assert!(t.app().error().is_none(), "{:?}", t.app().error());
}

#[test]
fn ctrl_f_fetches_from_any_pane() {
    let sb = ahead_of_origin();
    // Someone else pushes a new branch.
    let other = sb.clone_to(&sb.root().join("remotes").join("origin.git"), "other");
    sb.git_in(&other, &["switch", "-q", "-c", "brand-new"]);
    sb.git_in(&other, &["commit", "-q", "--allow-empty", "-m", "their work"]);
    sb.git_in(&other, &["push", "-q", "origin", "brand-new"]);
    let mut t = TestDriver::new(&sb);
    assert!(
        t.app().branches().visible().all(|e| e.name != "brand-new"),
        "not fetched yet"
    );
    // From the graph, where f opens the filter instead of fetching.
    t.press("f");
    assert!(matches!(t.app().modal(), Some(Modal::Filter(_))), "f still filters");
    t.press("esc");
    t.press("ctrl+f");
    assert_eq!(t.app().notice(), Some("✓ fetched all remotes"));
    assert!(
        t.app().branches().visible().any(|e| e.name == "brand-new"),
        "the new remote branch is listed"
    );
    assert!(t.app().error().is_none(), "{:?}", t.app().error());
}

#[test]
fn a_rejected_push_clears_the_spinner_and_shows_the_error() {
    let sb = ahead_of_origin();
    // Someone else pushes to main first.
    let other = sb.clone_to(&sb.root().join("remotes").join("origin.git"), "other");
    std::fs::write(other.join("b.txt"), "theirs").unwrap();
    sb.git_in(&other, &["add", "b.txt"]);
    sb.git_in(&other, &["commit", "-q", "-m", "their work"]);
    sb.git_in(&other, &["push", "-q"]);

    let mut t = TestDriver::new(&sb);
    t.press_frame("P");
    assert!(help_bar(&t).contains("⠋ pushing"), "{}", help_bar(&t));
    t.wait_idle();
    let error = t.app().error().unwrap_or_default();
    assert!(error.starts_with("push failed:"), "{error}");
    assert!(t.app().notice().is_none());
    assert!(!help_bar(&t).contains('│'), "{}", help_bar(&t));
}
