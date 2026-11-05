use crate::config::Config;
use crate::ui::root::RootView;
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::time::{Duration, Instant};

fn wait_until(cx: &mut VisualTestContext, view: &Entity<RootView>, secs: u64, cond: impl Fn(&RootView) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |v, _| cond(v)) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cursor_line(view: &RootView) -> String {
    let Some(content) = view.model.shown() else {
        return String::new();
    };
    content.cursor().and_then(|(line, _)| content.line_text(line)).unwrap_or_default()
}

fn char_before_cursor(view: &RootView) -> Option<char> {
    let content = view.model.shown()?;
    let (line, col) = content.cursor()?;
    content.cell(line, col.checked_sub(1)?).map(|c| c.ch)
}

fn screen_text(view: &RootView) -> String {
    let Some(content) = view.model.shown() else {
        return String::new();
    };
    (content.first_line..content.end_line()).filter_map(|l| content.line_text(l)).collect::<Vec<_>>().join("\n")
}

#[gpui::test]
#[ignore]
fn live_typing_prediction_and_password_safety(cx: &mut TestAppContext) {
    let window_id = std::env::var("REMOTE_ITERM_E2E_WINDOW").expect("scratch window id");
    let session_id = std::env::var("REMOTE_ITERM_E2E_SESSION").expect("scratch session id");
    let key = std::fs::read_to_string(dirs::home_dir().unwrap().join("Library/Application Support/remote-iterm/access-key")).unwrap();
    let config = Config { server_url: "http://localhost:7291".into(), key: Some(key.trim().into()), ..Config::default() };
    let (view, cx) = cx.add_window_view(move |window, cx| RootView::new(config, window, cx));

    assert!(wait_until(cx, &view, 10, |v| v.model.windows.iter().any(|w| w.id == window_id)), "state never arrived");
    view.update(cx, |v, cx| {
        let effects = v.model.select_window(&window_id);
        v.apply_effects(effects, cx);
    });
    let selected = view.read_with(cx, |v, _| v.model.session_id.clone());
    assert_eq!(selected.as_deref(), Some(session_id.as_str()), "refusing to type: app is not on the scratch session");
    assert!(wait_until(cx, &view, 10, |v| v.model.shown().is_some_and(|c| c.cursor().is_some())), "no content for scratch session");

    cx.simulate_keystrokes("ctrl-u");
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    cx.simulate_keystrokes("e");
    assert!(wait_until(cx, &view, 5, |v| char_before_cursor(v) == Some('e')), "first char never echoed");
    let started = Instant::now();
    cx.simulate_input("cho predicted");
    let shown_instantly = view.read_with(cx, |v, _| v.predictor.overlay().iter().map(|o| o.ch).collect::<String>());
    assert!(!shown_instantly.is_empty() && "cho predicted".ends_with(&shown_instantly), "prediction should draw typed chars before echo, got {shown_instantly:?}");
    eprintln!("drawn before echo: {shown_instantly:?}");
    assert!(wait_until(cx, &view, 5, |v| cursor_line(v).contains("echo predicted") && v.predictor.overlay().is_empty()), "echo never confirmed");
    eprintln!("confirmed 13 predicted chars in {:?}", started.elapsed());

    cx.simulate_keystrokes("ctrl-u");
    cx.simulate_input("sudo -k; sudo -v");
    cx.simulate_keystrokes("enter");
    assert!(wait_until(cx, &view, 10, |v| cursor_line(v).to_lowercase().contains("password")), "no password prompt");
    cx.simulate_input("hunter2xyz");
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    let (overlay, text) = view.read_with(cx, |v, _| (v.predictor.overlay(), screen_text(v)));
    cx.simulate_keystrokes("ctrl-c");
    assert!(overlay.is_empty(), "password characters were predicted onto the screen");
    assert!(!text.contains("hunter2xyz"), "password visible on screen");

    assert!(wait_until(cx, &view, 5, |v| !cursor_line(v).to_lowercase().contains("password")), "sudo prompt did not exit");
    cx.simulate_input("vim -u NONE -N");
    cx.simulate_keystrokes("enter");
    std::thread::sleep(Duration::from_millis(800));
    cx.run_until_parked();
    cx.simulate_input("jjjkkk");
    let vim_overlay = view.read_with(cx, |v, _| v.predictor.overlay());
    cx.simulate_keystrokes("escape");
    cx.simulate_input(":q!");
    cx.simulate_keystrokes("enter");
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    assert!(vim_overlay.is_empty(), "vim normal-mode keys were predicted");
}

#[gpui::test]
#[ignore]
fn live_reconnect_after_service_restart(cx: &mut TestAppContext) {
    let key = std::fs::read_to_string(dirs::home_dir().unwrap().join("Library/Application Support/remote-iterm/access-key")).unwrap();
    let config = Config { server_url: "http://localhost:7291".into(), key: Some(key.trim().into()), ..Config::default() };
    let (view, cx) = cx.add_window_view(move |window, cx| RootView::new(config, window, cx));
    assert!(wait_until(cx, &view, 10, |v| v.connected && v.model.shown().is_some()), "never connected");

    if std::env::var("REMOTE_ITERM_E2E_RESTART").is_ok() {
        let uid = String::from_utf8(std::process::Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
        std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{}/ca.jiye.remote-iterm", uid.trim())])
            .status()
            .unwrap();
        assert!(wait_until(cx, &view, 10, |v| !v.connected), "never noticed the restart");
        let lost = Instant::now();
        assert!(wait_until(cx, &view, 30, |v| v.connected), "never reconnected");
        eprintln!("reconnected {:?} after noticing the drop", lost.elapsed());
        let snapshot = view.read_with(cx, |v, _| v.model.shown().cloned());
        assert!(wait_until(cx, &view, 15, |v| v.model.shown().cloned() != snapshot), "no live content after reconnect");
    }
}
