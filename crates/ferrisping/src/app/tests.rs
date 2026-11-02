//! Drives the real window headlessly: typing, clicks, drags and menus.
//!
//! Hosts are loopback addresses, so these run without a network.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::time::Duration;

use eframe::App as _;
use eframe::egui::accesskit::{Role, Toggled};
use eframe::egui::{self, Modifiers, PointerButton, Pos2, Rect, Vec2, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use ferrisping_core::{
    EchoStatus, Event, EventKind, Exit, ExitChange, Host, HostStatus, Outcome, RECENT_RESULTS, TargetSpec, now_secs,
    parse_targets,
};

use super::{FerrisPingApp, LOOKUPS_LABEL, PRIVACY_URL, STORAGE_KEY, Saved};
use crate::args::Args;
use crate::lanes;
use crate::route::RouteView;
use crate::theme::ThemeChoice;
use crate::window;

fn app<'a>(hosts: &str) -> Harness<'a, FerrisPingApp> {
    app_with(Args {
        hosts: parse_targets(hosts),
        ..Args::default()
    })
}

fn app_with<'a>(args: Args) -> Harness<'a, FerrisPingApp> {
    app_sized(args, window::SIZE)
}

/// The app in a window whose content is `size` points.
fn app_sized<'a>(args: Args, size: Vec2) -> Harness<'a, FerrisPingApp> {
    app_saved(args, size, answered())
}

/// What any run after the first has saved: the notice about the network answered.
fn answered() -> Saved {
    Saved {
        notice_answered: true,
        ..Saved::default()
    }
}

/// The app as `saved` left it, in a window whose content is `size` points.
fn app_saved<'a>(args: Args, size: Vec2, saved: Saved) -> Harness<'a, FerrisPingApp> {
    let args = Args {
        window_secs: Some(60),
        theme: Some(ThemeChoice::Dark),
        ..args
    };
    // The harness runs each queued event in a frame of its own; at its default 0.25 s
    // per frame a double-click would be too slow to count, so step like a 60 Hz screen.
    let mut harness = Harness::builder()
        .with_size(size)
        .with_step_dt(1.0 / 60.0)
        .build_eframe(|cc| FerrisPingApp::with_saved(cc, args, saved));
    harness.run_steps(3);
    harness
}

/// The very first run: nothing saved, and no hosts on the command line.
fn first_run<'a>() -> Harness<'a, FerrisPingApp> {
    app_saved(Args::default(), window::SIZE, Saved::default())
}

fn hosts(harness: &Harness<'_, FerrisPingApp>) -> Vec<String> {
    let state = harness.state().monitor.read();
    state.hosts.iter().map(|h| h.spec.host.clone()).collect()
}

/// A point inside the graph of the lane whose name label is `name`.
fn graph_point(harness: &Harness<'_, FerrisPingApp>, name: &str) -> Pos2 {
    let label = harness.get_by_label(name).rect();
    pos2(label.left() + 480.0, label.center().y + 30.0)
}

fn click_at(harness: &Harness<'_, FerrisPingApp>, pos: Pos2, button: PointerButton) {
    harness.hover_at(pos);
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
}

fn interval_picker<'t>(harness: &'t Harness<'_, FerrisPingApp>) -> egui_kittest::Node<'t> {
    harness.get_by_role_and_label(Role::ComboBox, "Ping interval")
}

/// The host's name in its lane. With the route panel open the name shows twice,
/// and the lane is the upper one.
fn lane_name(harness: &Harness<'_, FerrisPingApp>, name: &str) -> Rect {
    harness
        .query_all_by_label(name)
        .map(|node| node.rect())
        .min_by(|a, b| a.top().total_cmp(&b.top()))
        .unwrap_or_else(|| panic!("no lane named {name}"))
}

/// Steps the app until `done` holds, giving the probes time between frames.
fn wait_for(harness: &mut Harness<'_, FerrisPingApp>, what: &str, done: impl Fn(&Harness<'_, FerrisPingApp>) -> bool) {
    for _ in 0..100 {
        if done(harness) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
        harness.step();
    }
    panic!("timed out waiting for {what}");
}

fn addresses_known(harness: &Harness<'_, FerrisPingApp>) -> bool {
    harness.state().monitor.read().hosts.iter().all(|h| h.addr.is_some())
}

fn route_reached(harness: &Harness<'_, FerrisPingApp>) -> bool {
    harness.query_by_label_contains("reached in 1 hop").is_some()
}

#[test]
fn typing_hosts_and_pressing_enter_adds_them() {
    let mut harness = app("");
    harness.get_by_role(Role::TextInput).click();
    harness.run_steps(2);
    harness.get_by_role(Role::TextInput).type_text("127.0.0.1, 127.0.0.2");
    harness.run_steps(1);
    harness.key_press(egui::Key::Enter);
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.1", "127.0.0.2"]);
    assert!(harness.state().add_text.is_empty(), "the box clears after adding");
}

#[test]
fn adding_the_same_host_twice_is_ignored() {
    let mut harness = app("127.0.0.1");
    harness.get_by_role(Role::TextInput).click();
    harness.run_steps(2);
    harness.get_by_role(Role::TextInput).type_text("127.0.0.1");
    harness.run_steps(1);
    harness.get_by_label("Add").click();
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.1"]);

    // Nor does one paste add a host twice.
    harness.get_by_role(Role::TextInput).click();
    harness.run_steps(2);
    harness
        .get_by_role(Role::TextInput)
        .type_text("127.0.0.2, 127.0.0.2; LOCALHOST, localhost");
    harness.run_steps(1);
    harness.get_by_label("Add").click();
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.1", "127.0.0.2", "LOCALHOST"]);
}

#[test]
fn dragging_a_graph_leaves_live_and_back_to_live_returns() {
    let mut harness = app("127.0.0.1, 127.0.0.2");
    assert!(harness.state().live);
    assert!(harness.query_by_label("Back to live").is_none());

    let start = graph_point(&harness, "127.0.0.1");
    harness.hover_at(start);
    harness.step();
    harness.drag_at(start);
    harness.step();
    for dx in [10.0, 60.0, 150.0] {
        harness.hover_at(start + vec2(dx, 0.0));
        harness.step();
    }
    harness.drop_at(start + vec2(150.0, 0.0));
    harness.run_steps(2);
    assert!(
        !harness.state().live,
        "dragging should stop following the newest samples"
    );

    harness.get_by_label("Back to live").click();
    harness.run_steps(2);
    assert!(harness.state().live);
    assert!(harness.query_by_label("Back to live").is_none());
}

#[test]
fn double_clicking_a_graph_returns_to_live() {
    let mut harness = app("127.0.0.1");
    harness.state_mut().live = false;
    harness.run_steps(2);
    let point = graph_point(&harness, "127.0.0.1");
    click_at(&harness, point, PointerButton::Primary);
    click_at(&harness, point, PointerButton::Primary);
    harness.run_steps(2);
    assert!(harness.state().live);
}

#[test]
fn right_clicking_a_host_name_opens_its_menu() {
    let mut harness = app("127.0.0.1, 127.0.0.2, 127.0.0.3");
    let name = harness.get_by_label("127.0.0.3").rect().center();
    click_at(&harness, name, PointerButton::Secondary);
    harness.run_steps(2);
    harness.get_by_label("Move up").click();
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.1", "127.0.0.3", "127.0.0.2"]);
}

#[test]
fn right_clicking_a_graph_opens_the_same_menu() {
    let mut harness = app("127.0.0.1, 127.0.0.2");
    let point = graph_point(&harness, "127.0.0.1");
    click_at(&harness, point, PointerButton::Secondary);
    harness.run_steps(2);
    assert!(
        harness.get_by_label("Move up").accesskit_node().is_disabled(),
        "the first host can't move up"
    );
    harness.get_by_label("Remove").click();
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.2"]);
}

#[test]
fn window_buttons_change_the_range_and_return_to_live() {
    let mut harness = app("127.0.0.1");
    harness.state_mut().live = false;
    harness.run_steps(1);
    harness.get_by_label("15m").click();
    harness.run_steps(2);
    assert_eq!(harness.state().window_secs, 900);
    assert!(harness.state().live);
}

#[test]
fn interval_picker_changes_the_probe_interval() {
    let mut harness = app("127.0.0.1");
    assert_eq!(interval_picker(&harness).value().as_deref(), Some("Every 1 s"));
    interval_picker(&harness).click();
    harness.run_steps(2);
    harness.get_by_label("5 s").click();
    harness.run_steps(2);
    assert_eq!(harness.state().monitor.settings().interval, Duration::from_secs(5));
    assert_eq!(interval_picker(&harness).value().as_deref(), Some("Every 5 s"));
}

#[test]
fn pause_and_resume() {
    let mut harness = app("127.0.0.1");
    harness.get_by_label("Pause").click();
    harness.run_steps(2);
    assert!(harness.state().monitor.settings().paused);
    harness.get_by_label("Resume").click();
    harness.run_steps(2);
    assert!(!harness.state().monitor.settings().paused);
}

#[test]
fn theme_menu_switches_theme() {
    let mut harness = app("127.0.0.1");
    harness.get_by_label("More options").click();
    harness.run_steps(2);
    harness.get_by_label("Light").click();
    harness.run_steps(2);
    assert_eq!(harness.state().theme, ThemeChoice::Light);
    assert_eq!(harness.ctx.theme(), egui::Theme::Light);
}

#[test]
fn events_toggle_hides_and_shows_the_feed() {
    let mut harness = app("127.0.0.1");
    assert!(harness.state().show_events);
    harness.get_by_role_and_label(Role::Button, "Events").click();
    harness.run_steps(2);
    assert!(!harness.state().show_events);
    harness.get_by_role_and_label(Role::Button, "Events").click();
    harness.run_steps(2);
    assert!(harness.state().show_events);
}

#[test]
fn trace_route_from_the_lane_menu_then_restart_and_close() {
    let mut harness = app("127.0.0.1");
    wait_for(&mut harness, "the address", addresses_known);
    let name = harness.get_by_label("127.0.0.1").rect().center();
    click_at(&harness, name, PointerButton::Secondary);
    harness.run_steps(2);
    harness.get_by_label("Trace route").click();
    harness.run_steps(2);
    assert!(harness.state().route.is_some(), "the route panel opens");
    wait_for(&mut harness, "the route", route_reached);

    let started = |h: &Harness<'_, FerrisPingApp>| h.state().route.as_ref().unwrap().trace.read().started;
    let before = started(&harness);
    harness.get_by_label("Restart").click();
    harness.run_steps(2);
    assert!(started(&harness) > before, "restart starts the numbers over");

    harness.get_by_label("Close route").click();
    harness.run_steps(2);
    assert!(harness.state().route.is_none());
    assert_eq!(hosts(&harness), ["127.0.0.1"], "closing the route keeps the host");
}

#[test]
fn route_button_on_a_lane_and_removing_the_host_closes_it() {
    let mut harness = app("127.0.0.1, 127.0.0.2");
    wait_for(&mut harness, "the addresses", addresses_known);
    harness.hover_at(lane_name(&harness, "127.0.0.2").center());
    harness.run_steps(2);
    harness.get_by_label("Trace route to 127.0.0.2").click();
    harness.run_steps(2);
    let target = harness
        .state()
        .route
        .as_ref()
        .map(|view| view.trace.target().to_string());
    assert_eq!(target.as_deref(), Some("127.0.0.2"));

    // The lanes got shorter to make room, so find the name again.
    harness.hover_at(lane_name(&harness, "127.0.0.2").center());
    harness.run_steps(2);
    harness.get_by_label("Remove 127.0.0.2").click();
    harness.run_steps(2);
    assert_eq!(hosts(&harness), ["127.0.0.1"]);
    assert!(harness.state().route.is_none(), "the route goes with its host");
}

#[test]
fn route_flag_opens_the_route_on_start() {
    let mut harness = app_with(Args::parse(["--route", "127.0.0.1"].into_iter().map(String::from)));
    assert_eq!(hosts(&harness), ["127.0.0.1"], "--route watches the host too");
    wait_for(&mut harness, "the route", route_reached);

    // A host written with a label, as in the add box, opens its route as well.
    let mut harness = app_with(Args::parse(
        ["--route", "127.0.0.1 Loopback"].into_iter().map(String::from),
    ));
    wait_for(&mut harness, "the route", route_reached);
}

#[test]
fn route_panel_fits_its_route_and_resizes_from_its_top_edge() {
    // Enough hosts that the lanes want all the room.
    let hosts = "127.0.0.1, 127.0.0.2, 127.0.0.3, 127.0.0.4, 127.0.0.5, 127.0.0.6";
    let mut harness = app_with(Args::parse(
        [hosts, "--route", "127.0.0.1"].into_iter().map(String::from),
    ));
    wait_for(&mut harness, "the route", route_reached);
    harness.run_steps(4);
    let edge = |h: &Harness<'_, FerrisPingApp>| h.get_by_label("Route panel size").rect();
    let fitted = edge(&harness).top();
    let status_bar = harness.get_by_role_and_label(Role::Button, "Events").rect().top();
    assert!(
        status_bar - fitted < 200.0,
        "a one-hop route gets a small panel, not {}",
        status_bar - fitted
    );

    let start = edge(&harness).center();
    harness.hover_at(start);
    harness.step();
    harness.drag_at(start);
    harness.step();
    for dy in [10.0, 40.0, 80.0] {
        harness.hover_at(start - vec2(0.0, dy));
        harness.step();
    }
    harness.drop_at(start - vec2(0.0, 80.0));
    harness.run_steps(3);
    let moved = fitted - edge(&harness).top();
    assert!(
        (moved - 80.0).abs() < 2.0,
        "the edge follows the pointer, but moved {moved}"
    );
    assert!(harness.state().route_height.is_some());

    let handle = edge(&harness).center();
    click_at(&harness, handle, PointerButton::Primary);
    click_at(&harness, handle, PointerButton::Primary);
    harness.run_steps(3);
    assert!(
        harness.state().route_height.is_none(),
        "double-click fits the route again"
    );
    assert!((edge(&harness).top() - fitted).abs() < 1.0);
}

/// The text the app put on the clipboard within a few frames, if any.
fn text_copied(harness: &mut Harness<'_, FerrisPingApp>) -> Option<String> {
    (0..4).find_map(|_| {
        harness.step();
        harness
            .output()
            .platform_output
            .commands
            .iter()
            .find_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
    })
}

#[test]
fn copy_report_puts_the_route_on_the_clipboard() {
    let mut harness = app_with(Args::parse(["--route", "127.0.0.1"].into_iter().map(String::from)));
    wait_for(&mut harness, "the route", route_reached);
    harness.get_by_label("Copy report").click();
    let report = text_copied(&mut harness).expect("the report goes to the clipboard");
    assert!(
        report.starts_with("FerrisPing route to 127.0.0.1 (127.0.0.1) since "),
        "{report}"
    );
    assert!(report.contains("reached in 1 hop"), "{report}");
    assert!(
        report.lines().any(|line| line.trim_start().starts_with("1  ")),
        "a line for hop 1:\n{report}"
    );
}

const LUMEN: &str = "LEVEL3 - Level 3 Parent, LLC, US";
const COMCAST: &str = "COMCAST-7922 - Comcast Cable Communications, LLC, US";
const CLOUDFLARE: &str = "CLOUDFLARENET - Cloudflare, Inc., US";

fn router(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[test]
fn the_way_out_shows_on_the_lane_and_its_network_can_be_renamed() {
    let mut harness = app("127.0.0.1");
    wait_for(&mut harness, "the address", addresses_known);
    let now = now_secs();
    harness.state().monitor.edit_for_test(|state| {
        let first = router("4.69.0.1");
        state.networks.insert_for_test(first, 3356, LUMEN);
        state.hosts[0].exit = Some(Exit {
            asn: 3356,
            router: first,
            hop: 3,
            since: now,
            seen: now,
        });
    });
    harness.run_steps(2);
    harness.get_by_label("via Lumen");

    click_at(
        &harness,
        lane_name(&harness, "127.0.0.1").center(),
        PointerButton::Secondary,
    );
    harness.run_steps(2);
    harness.get_by_label("Rename Lumen…").click();
    // A dialog stays hidden for its first frame, while egui measures it.
    harness.run_steps(4);
    // The name starts out selected, so typing replaces it.
    harness.get_by_label("Network name").type_text("Lumen (office)");
    harness.run_steps(1);
    harness.get_by_label("Save").click();
    harness.run_steps(2);
    assert!(harness.state().rename.is_none(), "the dialog closes");
    assert_eq!(harness.state().monitor.read().networks.name(3356), "Lumen (office)");
    harness.get_by_label("via Lumen (office)");

    // And back to the usual name.
    click_at(
        &harness,
        lane_name(&harness, "127.0.0.1").center(),
        PointerButton::Secondary,
    );
    harness.run_steps(2);
    harness.get_by_label("Rename Lumen (office)…").click();
    harness.run_steps(4);
    harness.get_by_label("Use its usual name").click();
    harness.run_steps(2);
    harness.get_by_label("via Lumen");
}

/// Settings kept in memory instead of on disk.
#[derive(Default)]
struct MemoryStorage(HashMap<String, String>);

impl eframe::Storage for MemoryStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }

    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }

    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }

    fn flush(&mut self) {}
}

#[test]
fn skipped_probes_show_in_the_status_bar() {
    let mut harness = app("127.0.0.1");
    assert!(harness.query_by_label("Skipping probes").is_none());
    harness
        .state()
        .monitor
        .edit_for_test(|state| state.probes_skipped_at = Some(now_secs()));
    harness.run_steps(2);
    harness.get_by_label("Skipping probes");
    // The note goes once probes stop being skipped.
    harness
        .state()
        .monitor
        .edit_for_test(|state| state.probes_skipped_at = Some(now_secs() - 60.0));
    harness.run_steps(2);
    assert!(harness.query_by_label("Skipping probes").is_none());
}

#[test]
fn saving_keeps_the_hosts_and_network_names() {
    // Without hosts on the command line, the window's hosts are the saved ones.
    let mut harness = app("");
    harness.state().monitor.add(TargetSpec::new("127.0.0.1"));
    harness
        .state()
        .monitor
        .set_network_name(3356, Some("Lumen (office)".to_string()));
    let mut storage = MemoryStorage::default();
    // Saving used to lock the state twice at once; debug builds now panic on that.
    harness.state_mut().save(&mut storage);
    let saved: Saved = eframe::get_value(&storage, STORAGE_KEY).expect("settings were saved");
    assert_eq!(saved.hosts, ["127.0.0.1"]);
    assert_eq!(
        saved.network_names,
        BTreeMap::from([(3356, "Lumen (office)".to_string())])
    );
}

#[test]
fn a_switch_to_another_isp_is_logged() {
    let mut harness = app("127.0.0.1");
    wait_for(&mut harness, "the address", addresses_known);
    let at = now_secs() - 20.0;
    harness.state().monitor.edit_for_test(|state| {
        let (lumen, comcast) = (router("4.69.0.1"), router("68.86.0.1"));
        state.networks.insert_for_test(lumen, 3356, LUMEN);
        state.networks.insert_for_test(comcast, 7922, COMCAST);
        let host = &mut state.hosts[0];
        host.exit = Some(Exit {
            asn: 7922,
            router: comcast,
            hop: 3,
            since: at,
            seen: at + 30.0,
        });
        host.exit_changes.push(ExitChange {
            at,
            from: 3356,
            to: 7922,
        });
        let event = Event {
            at,
            host: host.id,
            host_name: host.name().to_string(),
            kind: EventKind::ExitChanged { from: 3356, to: 7922 },
        };
        state.events.push(event);
    });
    harness.run_steps(2);
    harness.get_by_label("via Comcast");
    assert!(
        harness
            .query_by_label_contains("Now leaves via Comcast (was Lumen)")
            .is_some(),
        "the switch is in the events"
    );
}

#[test]
fn hovering_a_lane_keeps_its_text_still() {
    // Enough hosts for the shortest lanes that still show their numbers.
    let mut harness = app("127.0.0.1, 127.0.0.2, 127.0.0.3, 127.0.0.4, 127.0.0.5, 127.0.0.6, 127.0.0.7");
    wait_for(&mut harness, "the addresses", addresses_known);
    harness.run_steps(2);
    // The first lane's numbers: the top "Avg".
    let numbers = |h: &Harness<'_, FerrisPingApp>| {
        h.query_all_by_label("Avg")
            .map(|node| node.rect())
            .min_by(|a, b| a.top().total_cmp(&b.top()))
            .expect("a lane")
    };
    let before = numbers(&harness);

    harness.hover_at(graph_point(&harness, "127.0.0.1"));
    harness.run_steps(2);
    harness.get_by_label("Trace route to 127.0.0.1");
    assert_eq!(
        numbers(&harness),
        before,
        "the lane's buttons don't push its numbers down"
    );
}

#[test]
fn a_dozen_hosts_fit_on_a_laptop_screen() {
    // The first window on a 1280 × 800-point laptop screen, clear of the taskbar.
    let hosts: Vec<String> = (1..=12).map(|i| format!("127.0.0.{i}")).collect();
    let args = Args {
        hosts: parse_targets(&hosts.join(", ")),
        ..Args::default()
    };
    let mut harness = app_sized(args, vec2(1180.0, 712.0));
    wait_for(&mut harness, "the addresses", addresses_known);
    harness.run_steps(2);

    // Twelve lanes in the card, and the card ends above the status bar: no scrolling.
    let first = lane_name(&harness, "127.0.0.1");
    let pitch = lane_name(&harness, "127.0.0.2").top() - first.top();
    let card_top = harness.get_by_label("Ping interval").rect().bottom() + 16.0;
    let card_bottom = card_top + lanes::natural_height(0) + 12.0 * pitch;
    let status_bar = harness.get_by_role_and_label(Role::Button, "Events").rect().top();
    assert!(
        card_bottom <= status_bar - 16.0 + 0.5,
        "twelve {pitch}-point lanes end at {card_bottom}, past the status bar at {status_bar}"
    );

    // Compact lanes leave the numbers out, and the name's tooltip has them.
    assert!(harness.query_by_label("Avg").is_none(), "no numbers in the lanes");
    harness.hover_at(first.center());
    harness.run_steps(40);
    harness.get_by_label("Avg");
}

#[test]
fn a_lanes_address_shows_whole_or_not_at_all() {
    // With a label the lane shows the address too, after the network.
    let mut harness = app("127.0.0.1 Office edge");
    wait_for(&mut harness, "a reply", |h| {
        h.state()
            .monitor
            .read()
            .hosts
            .iter()
            .all(|h| h.status == HostStatus::Up)
    });
    let now = now_secs();
    harness.state().monitor.edit_for_test(|state| {
        let first = router("68.86.0.1");
        state.networks.insert_for_test(first, 7922, COMCAST);
        state.hosts[0].exit = Some(Exit {
            asn: 7922,
            router: first,
            hop: 3,
            since: now,
            seen: now,
        });
    });
    harness.run_steps(2);
    let address = |h: &Harness<'_, FerrisPingApp>| h.query_all_by_label_contains("127.0.0.1").next().is_some();
    assert!(address(&harness), "the address fits beside the status and the network");

    // The lane's buttons take the room, so the address steps aside instead of showing "·…".
    harness.hover_at(graph_point(&harness, "Office edge"));
    harness.run_steps(2);
    harness.get_by_label("Trace route to Office edge");
    assert!(!address(&harness), "no cut-off address");
}

#[test]
fn the_route_panel_names_each_hops_network() {
    let mut harness = app_with(Args::parse(["--route", "127.0.0.1"].into_iter().map(String::from)));
    wait_for(&mut harness, "the route", route_reached);
    harness.get_by_label("Network");
    // Loopback is private: it belongs to no network on the internet.
    harness.get_by_label("Private");
}

#[test]
fn route_panel_reaches_up_to_lanes_that_leave_room() {
    let mut harness = app_with(Args::parse(["--route", "127.0.0.1"].into_iter().map(String::from)));
    wait_for(&mut harness, "the route", route_reached);
    harness.run_steps(4);
    // One lane at full height ends 16 + 202 points under the toolbar.
    let lanes_end = harness.get_by_label("Ping interval").rect().bottom() + 16.0 + lanes::natural_height(1);
    let edge = harness.get_by_label("Route panel size").rect().top();
    assert!(
        (edge - lanes_end).abs() < 24.0,
        "the panel starts under the lanes at {lanes_end}, not {edge}"
    );
}

const NOTICE_TITLE: &str = "What FerrisPing sends over the network";

/// Whether routes are checked and networks looked up right now.
fn lookups_on(harness: &Harness<'_, FerrisPingApp>) -> bool {
    harness.state().monitor.settings().network_lookups
}

/// Whether the box for checking routes and looking up networks is ticked, in the
/// notice or the menu, whichever is open.
fn lookups_ticked(harness: &Harness<'_, FerrisPingApp>) -> bool {
    harness.get_by_label(LOOKUPS_LABEL).accesskit_node().toggled() == Some(Toggled::True)
}

fn open_more_options(harness: &mut Harness<'_, FerrisPingApp>) {
    harness.get_by_label("More options").click();
    harness.run_steps(2);
}

/// The address the app asked the browser to open within a few frames, if any.
fn url_opened(harness: &mut Harness<'_, FerrisPingApp>) -> Option<String> {
    (0..4).find_map(|_| {
        harness.step();
        harness
            .output()
            .platform_output
            .commands
            .iter()
            .find_map(|command| match command {
                egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
                _ => None,
            })
    })
}

#[test]
fn the_first_run_says_what_ferrisping_sends_before_sending_it() {
    let mut harness = first_run();
    harness.get_by_label(NOTICE_TITLE);
    assert!(lookups_ticked(&harness), "on unless it's turned off");
    assert!(
        !lookups_on(&harness),
        "nothing is checked or looked up before it's answered"
    );

    // The whole list is a click away.
    harness.get_by_label("ferrisping.com/#privacy").click();
    assert_eq!(url_opened(&mut harness).as_deref(), Some(PRIVACY_URL));

    harness.get_by_label("OK").click();
    harness.run_steps(2);
    assert!(harness.query_by_label(NOTICE_TITLE).is_none(), "the notice closes");
    assert!(lookups_on(&harness));
    let mut storage = MemoryStorage::default();
    harness.state_mut().save(&mut storage);
    let saved: Saved = eframe::get_value(&storage, STORAGE_KEY).expect("settings were saved");
    assert!(saved.notice_answered && saved.network_lookups);
}

#[test]
fn the_notice_comes_once_and_its_choice_is_kept() {
    let mut harness = first_run();
    harness.get_by_label(LOOKUPS_LABEL).click();
    harness.run_steps(2);
    assert!(!lookups_ticked(&harness));
    harness.get_by_label("OK").click();
    harness.run_steps(2);
    assert!(!lookups_on(&harness));
    let mut storage = MemoryStorage::default();
    harness.state_mut().save(&mut storage);
    drop(harness);

    // The next start reads the settings the way every start does.
    let mut harness = Harness::builder().with_size(window::SIZE).build_eframe(|cc| {
        cc.storage = Some(&storage);
        FerrisPingApp::new(cc, Args::default())
    });
    harness.run_steps(3);
    assert!(harness.query_by_label(NOTICE_TITLE).is_none(), "it isn't asked again");
    assert!(!lookups_on(&harness));
    open_more_options(&mut harness);
    assert!(!lookups_ticked(&harness), "the menu shows the choice");
}

#[test]
fn a_session_from_the_command_line_starts_without_the_notice() {
    let session = || Args {
        hosts: parse_targets("127.0.0.1"),
        ..Args::default()
    };
    // Even on the very first run, it starts as it always did.
    let harness = app_saved(session(), window::SIZE, Saved::default());
    assert!(harness.query_by_label(NOTICE_TITLE).is_none());
    assert!(lookups_on(&harness));
    // And it keeps to the choice made before.
    let off = Saved {
        network_lookups: false,
        ..answered()
    };
    let harness = app_saved(session(), window::SIZE, off);
    assert!(!lookups_on(&harness));
}

#[test]
fn turning_lookups_off_drops_the_way_out_and_its_switches() {
    let mut harness = app("127.0.0.1");
    wait_for(&mut harness, "the address", addresses_known);
    let now = now_secs();
    harness.state().monitor.edit_for_test(|state| {
        let first = router("4.69.0.1");
        state.networks.insert_for_test(first, 3356, LUMEN);
        state.hosts[0].exit = Some(Exit {
            asn: 3356,
            router: first,
            hop: 3,
            since: now,
            seen: now,
        });
    });
    harness.run_steps(2);
    harness.get_by_label("via Lumen");
    let switches_logged =
        |h: &Harness<'_, FerrisPingApp>| h.query_by_label_contains("switching to another ISP").is_some();
    assert!(switches_logged(&harness), "the events say switches are logged");

    open_more_options(&mut harness);
    assert!(lookups_ticked(&harness));
    harness.get_by_label(LOOKUPS_LABEL).click();
    harness.run_steps(2);
    assert!(!lookups_on(&harness));
    assert!(
        harness.query_by_label("via Lumen").is_none(),
        "nothing keeps the way out up to date"
    );
    assert!(!switches_logged(&harness), "nor are switches logged");

    // Clicking the box closed the menu.
    open_more_options(&mut harness);
    assert!(!lookups_ticked(&harness));
    harness.get_by_label(LOOKUPS_LABEL).click();
    harness.run_steps(2);
    assert!(lookups_on(&harness));
    assert!(switches_logged(&harness));
}

#[test]
fn with_lookups_off_an_open_route_names_no_networks() {
    let off = Saved {
        network_lookups: false,
        ..answered()
    };
    let mut harness = app_saved(Args::default(), window::SIZE, off);
    // A made-up route that's never probed: the home router, the ISP's, and a router
    // whose network was found before lookups were turned off.
    let monitor = &harness.state().monitor;
    monitor.edit_for_test(|state| state.networks.insert_for_test(router("4.69.0.1"), 3356, LUMEN));
    let host = monitor.add_for_test(TargetSpec::new("1.1.1.1"));
    let trace = monitor.trace_for_test(router("1.1.1.1"));
    trace.record_for_test(1, Some((router("192.168.1.1"), 1.0)));
    trace.record_for_test(2, Some((router("68.85.112.1"), 9.0)));
    trace.record_for_test(3, Some((router("4.69.0.1"), 12.0)));
    harness.state_mut().route = Some(RouteView { host, trace });
    harness.run_steps(3);
    harness.get_by_label("Private");
    assert_eq!(harness.query_all_by_label("Lookups off").count(), 2);
    assert!(
        harness.query_by_label_contains("Lumen").is_none(),
        "not even a network found before"
    );

    harness.get_by_label("Copy report").click();
    let report = text_copied(&mut harness).expect("the report goes to the clipboard");
    assert_eq!(report.matches("lookups off").count(), 2, "{report}");
    assert!(!report.contains("Lumen"), "{report}");
}

#[test]
fn the_notice_can_be_read_again_from_the_menu() {
    let mut harness = app("127.0.0.1");
    open_more_options(&mut harness);
    harness.get_by_label("What FerrisPing sends…").click();
    // A dialog stays hidden for its first frame, while egui measures it.
    harness.run_steps(4);
    harness.get_by_label(NOTICE_TITLE);
    assert!(lookups_ticked(&harness));
    harness.get_by_label(LOOKUPS_LABEL).click();
    harness.run_steps(2);
    assert!(lookups_on(&harness), "the box counts once the notice is closed");
    harness.get_by_label("OK").click();
    harness.run_steps(2);
    assert!(!lookups_on(&harness));
}

/// Renders a few states to PNG files for a visual check:
/// `FERRISPING_RENDER_DIR=<dir> cargo test -p ferrisping -- --ignored render_states`
#[test]
#[ignore = "writes images; run by hand"]
fn render_states() {
    let dir = std::path::PathBuf::from(std::env::var("FERRISPING_RENDER_DIR").expect("set FERRISPING_RENDER_DIR"));
    // Enough hosts for short lanes, where the layout is tightest.
    let mut harness = app("127.0.0.1, 127.0.0.2, 127.0.0.3, 127.0.0.4, 127.0.0.5, 127.0.0.6, 127.0.0.7, 127.0.0.8");
    // Let a few seconds of samples arrive.
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        harness.step();
    }
    let point = graph_point(&harness, "127.0.0.1") + vec2(200.0, 0.0);
    harness.hover_at(point);
    harness.run_steps(3);
    harness.render().unwrap().save(dir.join("ui-hover.png")).unwrap();

    // Short lanes keep their numbers in the name's tooltip.
    harness.hover_at(lane_name(&harness, "127.0.0.1").center());
    harness.run_steps(40);
    harness.render().unwrap().save(dir.join("ui-numbers.png")).unwrap();

    let name = harness.get_by_label("127.0.0.2").rect().center();
    click_at(&harness, name, PointerButton::Secondary);
    harness.run_steps(3);
    harness.render().unwrap().save(dir.join("ui-context-menu.png")).unwrap();
    harness.key_press(egui::Key::Escape);
    harness.run_steps(2);

    interval_picker(&harness).click();
    harness.run_steps(3);
    harness
        .render()
        .unwrap()
        .save(dir.join("ui-interval-menu.png"))
        .unwrap();
    harness.key_press(egui::Key::Escape);
    harness.run_steps(2);

    open_more_options(&mut harness);
    harness.run_steps(1);
    harness.render().unwrap().save(dir.join("ui-more-options.png")).unwrap();

    // What the first run shows before anything else.
    let mut harness = first_run();
    harness.run_steps(2);
    harness.render().unwrap().save(dir.join("ui-notice.png")).unwrap();

    // The shortest lanes that keep their numbers, and a dozen compact ones in the
    // first window on a 1280 × 800-point laptop screen.
    for (file, count, height) in [("ui-full-lanes.png", 6, 697.0), ("ui-laptop.png", 12, 712.0)] {
        let hosts: Vec<String> = (1..=count).map(|i| format!("127.0.0.{i}")).collect();
        let args = Args {
            hosts: parse_targets(&hosts.join(", ")),
            ..Args::default()
        };
        let mut harness = app_sized(args, vec2(1180.0, height));
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            harness.step();
        }
        harness.render().unwrap().save(dir.join(file)).unwrap();
    }
}

/// Renders lanes after a made-up switch from Lumen to Comcast, with its event and
/// graph marker:
/// `FERRISPING_RENDER_DIR=<dir> cargo test -p ferrisping -- --ignored render_switch`
#[test]
#[ignore = "writes an image; run by hand"]
fn render_switch() {
    let dir = std::path::PathBuf::from(std::env::var("FERRISPING_RENDER_DIR").expect("set FERRISPING_RENDER_DIR"));
    let mut harness = app("127.0.0.1, 127.0.0.2");
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(100));
        harness.step();
    }
    let at = now_secs() - 12.0;
    harness.state().monitor.edit_for_test(|state| {
        let (lumen, comcast) = (router("4.69.0.1"), router("68.86.0.1"));
        state.networks.insert_for_test(lumen, 3356, LUMEN);
        state.networks.insert_for_test(comcast, 7922, COMCAST);
        state.hosts[0].exit = Some(Exit {
            asn: 3356,
            router: lumen,
            hop: 3,
            since: at - 600.0,
            seen: at + 30.0,
        });
        let host = &mut state.hosts[1];
        host.exit = Some(Exit {
            asn: 7922,
            router: comcast,
            hop: 3,
            since: at,
            seen: at + 30.0,
        });
        host.exit_changes.push(ExitChange {
            at,
            from: 3356,
            to: 7922,
        });
        let event = Event {
            at,
            host: host.id,
            host_name: host.name().to_string(),
            kind: EventKind::ExitChanged { from: 3356, to: 7922 },
        };
        state.events.push(event);
    });
    harness.run_steps(3);
    harness.render().unwrap().save(dir.join("ui-switch.png")).unwrap();
}

/// Renders a real route in a window tall enough to show all of it:
/// `FERRISPING_RENDER_DIR=<dir> FERRISPING_ROUTE=google.com cargo test -p ferrisping -- --ignored render_route`
#[test]
#[ignore = "needs the internet and writes an image; run by hand"]
fn render_route() {
    let dir = std::path::PathBuf::from(std::env::var("FERRISPING_RENDER_DIR").expect("set FERRISPING_RENDER_DIR"));
    let host = std::env::var("FERRISPING_ROUTE").unwrap_or_else(|_| "google.com".into());
    let args = Args {
        window_secs: Some(60),
        theme: Some(ThemeChoice::Dark),
        ..Args::parse(["1.1.1.1", "--route", &host].into_iter().map(String::from))
    };
    let mut harness = Harness::builder()
        .with_size(vec2(1180.0, 1400.0))
        .build_eframe(|cc| FerrisPingApp::new(cc, args));
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(100));
        harness.step();
    }
    harness
        .render()
        .unwrap()
        .save(dir.join(format!("ui-route-{host}.png")))
        .unwrap();
}

/// Renders the screenshots in `docs/screenshots` from made-up hosts and a made-up
/// route, so they show no real network. Nothing is probed:
/// `FERRISPING_RENDER_DIR=<dir> cargo test -p ferrisping -- --ignored render_screenshots`
#[test]
#[ignore = "writes images; run by hand"]
fn render_screenshots() {
    let dir = std::path::PathBuf::from(std::env::var("FERRISPING_RENDER_DIR").expect("set FERRISPING_RENDER_DIR"));
    for (theme, lanes_file, route_file) in [
        (ThemeChoice::Dark, "ferrisping-dark.png", "ferrisping-route.png"),
        (ThemeChoice::Light, "ferrisping-light.png", "ferrisping-route-light.png"),
    ] {
        let mut harness = screenshot_app(theme, 15 * 60, vec2(1180.0, 800.0));
        rolling_update(&harness);
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(lanes_file)).unwrap();

        let mut harness = screenshot_app(theme, 60, vec2(1180.0, 860.0));
        route_to_cloudflare(&mut harness);
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(route_file)).unwrap();
    }
}

/// The app with no hosts, showing `window_secs` of graph, drawn at 1.5 pixels a point.
fn screenshot_app<'a>(theme: ThemeChoice, window_secs: u64, size: Vec2) -> Harness<'a, FerrisPingApp> {
    let args = Args {
        window_secs: Some(window_secs),
        theme: Some(theme),
        ..Args::default()
    };
    // The harness sets its own theme, so it needs telling too.
    let egui_theme = if theme == ThemeChoice::Light {
        egui::Theme::Light
    } else {
        egui::Theme::Dark
    };
    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.5)
        .with_theme(egui_theme)
        .build_eframe(|cc| FerrisPingApp::with_saved(cc, args, answered()));
    harness.run_steps(3);
    harness
}

/// Made-up round trips that come out the same on every run: about `base_ms`,
/// give or take `spread_ms`, with the odd spike.
struct Jitter {
    seed: u64,
    base_ms: f32,
    spread_ms: f32,
}

impl Jitter {
    fn new(seed: u64, base_ms: f32, spread_ms: f32) -> Self {
        Self {
            seed: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
            base_ms,
            spread_ms,
        }
    }

    /// A number from 0 up to 1 (xorshift).
    fn unit(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 40) as f32 / (1u64 << 24) as f32
    }

    fn next_ms(&mut self) -> f32 {
        let wobble = (self.unit() + self.unit() - 1.0) * self.spread_ms;
        let spike = if self.unit() < 0.02 {
            self.unit() * self.spread_ms * 5.0
        } else {
            0.0
        };
        (self.base_ms + wobble + spike).max(0.1)
    }
}

/// Gives `host` its address and a made-up probe a second for the last `secs`
/// seconds, lost while `down` says it was down.
fn make_up_samples(host: &mut Host, now: f64, secs: u32, mut jitter: Jitter, down: impl Fn(f64) -> bool) {
    host.addr = host.spec.host.parse().ok();
    for ago in (0..secs).rev() {
        let t = now - f64::from(ago) - 0.5;
        let seq = host.history.push_pending(t);
        let outcome = if down(t) {
            Outcome::Lost(EchoStatus::TimedOut)
        } else {
            Outcome::Reply {
                rtt_ms: jitter.next_ms(),
            }
        };
        host.history.complete(seq, outcome);
    }
    host.version += 1;
}

/// Six lanes in the middle of a rolling update: esxi-01 and esxi-02 have restarted,
/// and esxi-03 is restarting now.
fn rolling_update(harness: &Harness<'_, FerrisPingApp>) {
    // Each host's round trip and how much it varies, and when it went down and came
    // back up, in seconds ago.
    let hosts = [
        ("10.0.10.5 vcenter", 0.7, 0.3, None),
        ("10.0.10.11 esxi-01", 0.4, 0.2, Some((840.0, Some(610.0)))),
        ("10.0.10.12 esxi-02", 0.4, 0.2, Some((545.0, Some(295.0)))),
        ("10.0.10.13 esxi-03", 0.5, 0.2, Some((75.0, None))),
        ("10.0.10.14 esxi-04", 0.4, 0.2, None),
        ("1.1.1.1 Cloudflare DNS", 13.0, 1.5, None),
    ];
    let monitor = &harness.state().monitor;
    for (input, ..) in hosts {
        monitor.add_for_test(parse_targets(input).remove(0));
    }
    let now = now_secs();
    monitor.edit_for_test(|state| {
        let mut events = Vec::new();
        for (i, (host, &(_, base_ms, spread_ms, restart))) in state.hosts.iter_mut().zip(&hosts).enumerate() {
            let down = |t: f64| {
                restart.is_some_and(|(went, back): (f64, Option<f64>)| {
                    t >= now - went && back.is_none_or(|back| t < now - back)
                })
            };
            make_up_samples(host, now, 15 * 60, Jitter::new(i as u64 + 1, base_ms, spread_ms), down);
            (host.status, host.status_since) = match restart {
                None => (HostStatus::Up, now - 6.0 * 3600.0),
                Some((went, None)) => (HostStatus::Down, now - went),
                Some((_, Some(back))) => (HostStatus::Up, now - back),
            };
            if let Some((went, back)) = restart {
                let event = |at: f64, kind: EventKind| Event {
                    at,
                    host: host.id,
                    host_name: host.name().to_string(),
                    kind,
                };
                events.push(event(now - went, EventKind::Down));
                if let Some(back) = back {
                    events.push(event(
                        now - back,
                        EventKind::Up {
                            outage_secs: went - back,
                        },
                    ));
                }
            }
        }
        events.sort_by(|a, b| a.at.total_cmp(&b.at));
        state.events.extend(events);

        let first = router("68.85.112.1");
        state.networks.insert_for_test(first, 7922, COMCAST);
        state.hosts[5].exit = Some(Exit {
            asn: 7922,
            router: first,
            hop: 3,
            since: now - 6.0 * 3600.0,
            seen: now - 10.0,
        });
    });
}

/// Three lanes, and the route to Cloudflare's DNS through a cable ISP, with a hop
/// that never answers and one that answers only some probes.
fn route_to_cloudflare(harness: &mut Harness<'_, FerrisPingApp>) {
    let hosts = [
        ("192.168.1.1 Gateway", 1.4, 0.6),
        ("1.1.1.1 Cloudflare DNS", 13.0, 1.5),
        ("8.8.8.8 Google DNS", 15.0, 1.5),
    ];
    let monitor = &harness.state().monitor;
    for (input, ..) in hosts {
        monitor.add_for_test(parse_targets(input).remove(0));
    }
    let now = now_secs();
    let first = router("68.85.112.1");
    monitor.edit_for_test(|state| {
        for (i, (host, &(_, base_ms, spread_ms))) in state.hosts.iter_mut().zip(&hosts).enumerate() {
            let jitter = Jitter::new(i as u64 + 11, base_ms, spread_ms);
            make_up_samples(host, now, 120, jitter, |_| false);
            (host.status, host.status_since) = (HostStatus::Up, now - 3.0 * 3600.0);
            // The gateway is on this side of the ISP.
            if i > 0 {
                host.exit = Some(Exit {
                    asn: 7922,
                    router: first,
                    hop: 3,
                    since: now - 3.0 * 3600.0,
                    seen: now - 10.0,
                });
            }
        }
        for addr in ["68.85.112.1", "68.86.90.25", "68.86.85.130", "68.86.166.42"] {
            state.networks.insert_for_test(router(addr), 7922, COMCAST);
        }
        for addr in ["172.71.144.3", "1.1.1.1"] {
            state.networks.insert_for_test(router(addr), 13335, CLOUDFLARE);
        }
    });

    let target = router("1.1.1.1");
    let trace = monitor.trace_for_test(target);
    // Who answers at each hop, about how fast, and what share of the probes.
    let hops = [
        (Some("192.168.1.1"), 1.2, 1.0),
        (None, 0.0, 0.0),
        (Some("68.85.112.1"), 9.0, 1.0),
        (Some("68.86.90.25"), 10.5, 1.0),
        (Some("68.86.85.130"), 11.5, 0.65),
        (Some("68.86.166.42"), 13.5, 1.0),
        (Some("172.71.144.3"), 12.8, 1.0),
        (Some("1.1.1.1"), 13.0, 1.0),
    ];
    let mut jitters: Vec<Jitter> = (0..)
        .zip(&hops)
        .map(|(i, &(_, base_ms, _))| Jitter::new(i + 21, base_ms, 1.0))
        .collect();
    for _ in 0..RECENT_RESULTS {
        for (ttl, (&(addr, _, share), jitter)) in (1..).zip(hops.iter().zip(&mut jitters)) {
            let answered = jitter.unit() < share;
            let answer = addr.filter(|_| answered).map(|addr| (router(addr), jitter.next_ms()));
            trace.record_for_test(ttl, answer);
        }
    }
    trace.name_for_test(target, "one.one.one.one");
    let host = monitor.read().hosts[1].id;
    harness.state_mut().route = Some(RouteView { host, trace });
}
