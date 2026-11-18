//! UI: the tab bar as the window's title bar (#85), in Rune's headless `Runtime`: the tab strip
//! keeps clear of the macOS traffic lights by the inset Rune reports, the lights centre on the bar,
//! and the bar's empty part drags the window while its tabs and fields keep their presses.

use std::sync::Arc;

use rune::runtime::ElementInfo;
use rune::{Env, Runtime, TitlebarAction};

use akshually_core::{ReviewRequest, Service};
use akshually_testkit::TestRepo;
use akshually_ui::{State, WINDOW_CHROME, view};

/// A headless window over an empty service, showing the Inbox, its first frame built with
/// `inset` as the title-bar inset Rune reports: the traffic lights' trailing edge in a window,
/// 0 in full screen.
fn window(inset: f32) -> Runtime<State> {
    window_over(&Arc::new(Service::new()), inset)
}

/// `window`, over `service`.
fn window_over(service: &Arc<Service>, inset: f32) -> Runtime<State> {
    let mut rt = Runtime::new(State::new(service.clone()), view, (1280.0, 800.0), Env::default());
    rt.set_titlebar_inset(inset);
    rt.frame(0.0);
    rt
}

/// The element keyed `key`, as laid out in the last frame.
fn element(rt: &Runtime<State>, key: &str) -> ElementInfo {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key) && !e.exiting)
        .unwrap_or_else(|| panic!("no `{key}`"))
}

#[test]
fn the_inbox_tab_starts_past_the_traffic_lights() {
    let inset = WINDOW_CHROME.titlebar_inset();
    let rt = window(inset);

    let tab = element(&rt, "t-inbox");

    assert!(
        tab.rect[0] >= inset,
        "the Inbox tab starts at {} under the traffic lights, which end at {inset}",
        tab.rect[0]
    );
}

#[test]
fn in_full_screen_the_inbox_tab_goes_back_to_the_leading_edge() {
    let rt = window(0.0);

    let tab = element(&rt, "t-inbox");

    let inset = WINDOW_CHROME.titlebar_inset();
    assert!(tab.rect[0] < inset, "the Inbox tab starts at {}, still clear of lights that end at {inset}", tab.rect[0]);
}

#[test]
fn the_traffic_lights_are_vertically_centred_on_the_tab_bar() {
    let rt = window(WINDOW_CHROME.titlebar_inset());
    let bar = element(&rt, "tabbar").rect;

    let lights = WINDOW_CHROME.traffic_light_rects().expect("the window draws its content under the traffic lights");

    for [_, y, _, h] in lights {
        assert_eq!(y + h / 2.0, bar[1] + bar[3] / 2.0, "a traffic light isn't centred on the tab bar {bar:?}");
    }
}

#[test]
fn a_press_on_the_empty_part_of_the_tab_bar_drags_the_window() {
    let mut rt = window(WINDOW_CHROME.titlebar_inset());
    let bar = element(&rt, "tabbar").rect;
    // Past the tabs, at the bar's trailing end.
    let (x, y) = (bar[0] + bar[2] - 4.0, bar[1] + bar[3] / 2.0);

    rt.pointer_down(x, y);

    assert_eq!(
        rt.take_titlebar_action(),
        Some(TitlebarAction::Drag),
        "the tab bar's empty part doesn't drag the window"
    );
}

#[test]
fn a_press_on_a_tab_leaves_the_window_where_it_is() {
    let mut rt = window(WINDOW_CHROME.titlebar_inset());
    let (x, y) = rt.find("t-inbox").expect("no Inbox tab");

    rt.pointer_down(x, y);

    assert_eq!(rt.take_titlebar_action(), None, "a press on the Inbox tab drags the window");
}

#[test]
fn a_press_on_the_pull_request_opener_focuses_it_rather_than_drag_the_window() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n").commit_all("base");
    repo.write("a.txt", "two\n");
    let service = Arc::new(Service::new());
    // A Session shows, and with it the Pull Request opener.
    let _review = service.review(ReviewRequest::new(repo.path())).unwrap();
    let mut rt = window_over(&service, WINDOW_CHROME.titlebar_inset());
    let bar = element(&rt, "tabbar").rect;
    let field = rt
        .elements()
        .into_iter()
        .find(|e| e.role == "textbox" && e.rect[1] < bar[1] + bar[3])
        .expect("no Pull Request opener in the tab bar");
    let (x, y) = (field.rect[0] + field.rect[2] / 2.0, field.rect[1] + field.rect[3] / 2.0);

    rt.pointer_down(x, y);
    let action = rt.take_titlebar_action();
    rt.pointer_up(x, y);
    rt.frame(0.0);

    assert_eq!(action, None, "a press on the Pull Request opener drags the window");
    let focused = rt.elements().into_iter().find(|e| e.focused).map(|e| e.role);
    assert_eq!(focused, Some("textbox"), "the Pull Request opener didn't take focus");
}
