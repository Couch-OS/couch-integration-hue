//! The package against a bridge, through a real subprocess.
//!
//! The first case is the admission case a package with children would run if
//! it could: `couch_plugin::testing_v3::children` makes exactly these
//! assertions, and it cannot be used here because it starts the package with
//! no credential and this package's manifest says `pairing.required`. Every
//! assertion it makes is made below, by name, against `Endpoint::start_paired`
//! and `couch_plugin::list_children`.
//!
//! The rest is what a bridge does that a fixture in memory does not: a listing
//! that races an event stream, a read that must not become a request, a cache
//! that goes stale, a lamp that cannot do what it was asked, a room that takes
//! one command a second, and a key that stops working.
//!
//! Everything here talks to `tests/fake/bridge.rs` on 127.0.0.1 and to nothing
//! else.

mod fake;

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use couch_plugin::{list_children, Child, Endpoint, Error, Failure, Request, Response, Status};
use couch_sdk::{Credential, LightState, TypedAction, MAX_PAGE};
use fake::{
    bridge::{self, FakeBridge},
    slot,
};

const PATIENCE: Duration = Duration::from_secs(8);

fn paired(bridge: &FakeBridge, slot: &slot::Slot) -> Endpoint {
    slot.endpoint(
        bridge.settings_value(),
        Some(&bridge.credential()),
        PATIENCE,
    )
}

fn set_light(
    on: Option<bool>,
    brightness: Option<u8>,
    mirek: Option<u16>,
    xy: Option<(u16, u16)>,
) -> TypedAction {
    TypedAction::SetLight {
        on,
        brightness,
        mirek,
        xy,
    }
}

fn ask(endpoint: &Endpoint, kind: &str, request: Request) -> Result<Response, Failure> {
    endpoint.request_child_detailed(Some(kind), request)
}

fn light_state(response: Response) -> LightState {
    match response {
        Response::Status { status } => status.light.expect("a lamp's state"),
        other => panic!("expected a status, got {other:?}"),
    }
}

fn status_of(endpoint: &Endpoint, kind: &str, id: &str) -> Status {
    match ask(endpoint, kind, Request::status().at(id)).expect("a child's status") {
        Response::Status { status } => status,
        other => panic!("expected a status, got {other:?}"),
    }
}

/// Every child, and how many pages it took.
fn listed(endpoint: &Endpoint) -> (Vec<Child>, usize) {
    let mut pages = 0;
    let mut request = |r| {
        pages += 1;
        endpoint.request_detailed(r)
    };
    let children = list_children(&mut request).expect("the bridge lists its children");
    (children, pages)
}

/// Wait until the bridge has stopped being read: the session reads it once or
/// twice on the way up, and a test that measures requests has to start from a
/// quiet bridge.
fn settle(bridge: &FakeBridge) {
    let mut last = bridge.requests().len();
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(250));
        let now = bridge.requests().len();
        if now == last {
            thread::sleep(Duration::from_millis(500));
            if bridge.requests().len() == now {
                return;
            }
        }
        last = bridge.requests().len();
    }
    panic!("the bridge never stopped being read");
}

#[test]
fn listing_this_bridge_makes_every_assertion_the_admission_case_makes() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);

    let (first, pages) = listed(&endpoint);
    // 48 lamps, 14 rooms and 181 scenes: the size of the bridge this package
    // was written against. A zone is not a child; a zone's scenes are.
    assert_eq!(first.len(), bridge::CHILDREN);
    assert_eq!(first.len(), 243);
    let counted = |kind: &str| first.iter().filter(|child| child.kind == kind).count();
    assert_eq!(counted("light"), bridge::LIGHTS);
    assert_eq!(counted("group"), bridge::ROOMS);
    assert_eq!(counted("scene"), bridge::SCENES);
    // The listing ended, and it took as many pages as 243 children need: a
    // bridge that answered everything in one oversized page never got here.
    assert_eq!(pages, first.len().div_ceil(MAX_PAGE));
    assert_eq!(pages, 8);
    // Every kind is one the manifest declares.
    assert!(first
        .iter()
        .all(|child| slot.manifest.child_kind(&child.kind).is_some()));
    // Every lamp says which room the bridge keeps it in, which is what the
    // device picker pre-filters on.
    assert!(
        first
            .iter()
            .filter(|child| child.kind == "light")
            .all(|child| child.room_hint.is_some()),
        "every light has a room hint"
    );
    assert!(first
        .iter()
        .any(|child| child.room_hint.as_deref() == Some(bridge::LIVING_ROOM)));
    // A zone's scenes are listed under the zone's name.
    assert!(first
        .iter()
        .any(|child| child.kind == "scene" && child.room_hint.as_deref() == Some("Downstairs")));

    // Asked again: the same children, in the same order, under the same ids.
    // An id a browser saved yesterday has to mean the same lamp today.
    let (again, _) = listed(&endpoint);
    assert_eq!(
        first.iter().map(|c| &c.id).collect::<Vec<_>>(),
        again.iter().map(|c| &c.id).collect::<Vec<_>>(),
        "the listing is not stable"
    );
    assert_eq!(first, again, "a child changed between two listings");

    // A resource the bridge has never heard of is refused, never answered for
    // something else.
    assert!(
        ask(&endpoint, "light", Request::status().at("no-such-lamp")).is_err(),
        "an unknown resource was answered"
    );

    // The acknowledgement of a write is the state the child is in, and the
    // next read agrees with it.
    let lamp = bridge.light_child(0);
    assert!(bridge::dimmable(0));
    let written = ask(
        &endpoint,
        "light",
        Request::action(set_light(Some(true), Some(40), None, None)).at(&lamp),
    )
    .expect("a declared action on a child of its own kind");
    let read = status_of(&endpoint, "light", &lamp);
    match written {
        Response::Status { status } => assert_eq!(
            status, read,
            "the acknowledged state is not the state the child is in"
        ),
        Response::Ok => panic!("a write to a lamp should be acknowledged with its state"),
        other => panic!("a write was answered with {other:?}"),
    }

    // A kind that does not declare this action is never asked to perform it,
    // whichever child is named: the gate decides from the kind the saved
    // configuration holds, before any I/O.
    for id in [bridge.scene_child(0), lamp.clone()] {
        assert_eq!(
            ask(
                &endpoint,
                "scene",
                Request::action(set_light(Some(true), None, None, None)).at(&id)
            )
            .map_err(|failure| failure.code),
            Err(Error::Unsupported),
            "a scene answered an action only a lamp declares"
        );
    }
    // Without a kind at all, nothing is sent.
    assert_eq!(
        endpoint
            .request_detailed(Request::status().at(&lamp))
            .map_err(|failure| failure.code),
        Err(Error::Invalid),
        "a resource with no kind must not reach the package"
    );
}

#[test]
fn a_listing_taken_while_the_event_stream_is_busy_has_no_child_twice() {
    let bridge = Arc::new(FakeBridge::start());
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let (quiet, _) = listed(&endpoint);

    // Every event makes the session re-read the bridge, so without a frozen
    // snapshot the order could change between two pages of one listing - and
    // a child that came back twice costs this package its process, because
    // `list_children` calls that a protocol error.
    let stop = Arc::new(AtomicBool::new(false));
    let nudging = {
        let bridge = bridge.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                bridge.nudge();
                thread::sleep(Duration::from_millis(15));
            }
        })
    };
    let (busy, pages) = listed(&endpoint);
    stop.store(true, Ordering::SeqCst);
    nudging.join().expect("the nudging thread");

    assert_eq!(pages, 8);
    let ids: HashSet<&String> = busy.iter().map(|child| &child.id).collect();
    assert_eq!(ids.len(), busy.len(), "a child was listed twice");
    assert_eq!(
        busy.iter().map(|c| &c.id).collect::<Vec<_>>(),
        quiet.iter().map(|c| &c.id).collect::<Vec<_>>(),
        "the listing moved while events were arriving"
    );
}

#[test]
fn a_status_read_is_answered_from_the_cache_and_touches_no_network() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let lamp = bridge.light_child(0);
    // Warm the cache, then wait until the bridge has stopped being read.
    listed(&endpoint);
    status_of(&endpoint, "light", &lamp);
    settle(&bridge);

    // Now the bridge answers nothing at all. A read that went to it would
    // wait five seconds and then fail; a read from the cache cannot notice.
    bridge.silent();
    bridge.clear_log();
    let mut slowest = Duration::ZERO;
    for _ in 0..20 {
        let started = Instant::now();
        let status = status_of(&endpoint, "light", &lamp);
        slowest = slowest.max(started.elapsed());
        assert!(status.light.is_some(), "a lamp reports its state");
    }
    assert!(
        slowest < Duration::from_millis(50),
        "a status read took {slowest:?}; it must not be a request"
    );
    assert!(
        bridge.requests().is_empty(),
        "a status read reached the bridge: {:?}",
        bridge.requests()
    );
}

#[test]
fn a_cache_that_cannot_be_refreshed_becomes_a_transport_error() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let lamp = bridge.light_child(0);
    assert!(status_of(&endpoint, "light", &lamp).light.is_some());

    // The bridge goes away. The event stream ends, the poll that follows it
    // cannot connect, and a read has to say so rather than serve what the
    // bridge said a minute ago.
    bridge.close();
    let deadline = Instant::now() + Duration::from_secs(20);
    let failure = loop {
        match ask(&endpoint, "light", Request::status().at(&lamp)) {
            Err(failure) => break failure,
            Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(200)),
            Ok(_) => panic!("a stale cache went on answering"),
        }
    };
    assert_eq!(failure.code, Error::Transport);
}

#[test]
fn what_a_lamp_can_do_decides_what_it_is_asked_and_the_rest_costs_no_request() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let write = |id: &str, action| ask(&endpoint, "light", Request::action(action).at(id));

    // A lamp that dims, and can be told a colour temperature.
    let tunable = bridge.light_child(0);
    assert!(bridge::dimmable(0) && bridge::tunable(0));
    let state = light_state(write(&tunable, set_light(None, Some(64), None, None)).unwrap());
    assert_eq!(state.on, Some(true), "dimming a lamp turns it on");
    assert_eq!(state.brightness, Some(64));
    assert_eq!(
        bridge.resource("light", &bridge.light_id(0))["dimming"]["brightness"],
        64.0
    );

    // A colour temperature outside this lamp's range is clamped to it, not
    // refused: the person moved a slider, and the lamp has an end.
    let state = light_state(write(&tunable, set_light(None, None, Some(120), None)).unwrap());
    assert_eq!(state.mirek, Some(153), "clamped to the lamp's own range");
    assert_eq!(
        bridge.resource("light", &bridge.light_id(0))["color_temperature"]["mirek"],
        153
    );
    assert_eq!(
        status_of(&endpoint, "light", &tunable).light.unwrap(),
        state,
        "the read after a write agrees with it"
    );

    // Brightness zero is off, and the level the lamp comes back to is kept.
    let state = light_state(write(&tunable, set_light(None, Some(0), None, None)).unwrap());
    assert_eq!(state.on, Some(false));
    assert_eq!(
        state.brightness,
        Some(64),
        "Hue keeps the level it will resume"
    );

    // Colour: this version has none, and says so.
    bridge.clear_log();
    let refusal = write(&tunable, set_light(None, None, None, Some((4100, 3800)))).unwrap_err();
    assert_eq!(refusal.code, Error::Invalid);
    assert!(refusal.reason.unwrap().text().contains("colour"));

    // A lamp with no dimming service, and one with no colour temperature.
    let plain = bridge.light_child(7);
    assert!(!bridge::dimmable(7));
    let refusal = write(&plain, set_light(None, Some(40), None, None)).unwrap_err();
    assert_eq!(refusal.code, Error::Invalid);
    assert!(refusal.reason.unwrap().text().contains("dimmed"));
    // ...but it can still be switched off by a brightness of zero.
    assert_eq!(
        light_state(write(&plain, set_light(None, Some(0), None, None)).unwrap()).on,
        Some(false)
    );

    let warm = bridge.light_child(1);
    assert!(!bridge::tunable(1));
    let refusal = write(&warm, set_light(None, None, Some(300), None)).unwrap_err();
    assert_eq!(refusal.code, Error::Invalid);
    assert!(refusal
        .reason
        .unwrap()
        .text()
        .contains("colour temperature"));

    // None of the three refusals reached the bridge; only the one write did.
    let puts: Vec<String> = bridge
        .requests()
        .into_iter()
        .filter(|line| line.starts_with("PUT"))
        .collect();
    assert_eq!(puts.len(), 1, "a refusal must cost no round trip: {puts:?}");
}

#[test]
fn on_off_and_toggle_come_from_what_the_cache_says_the_lamp_is() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let command =
        |id: &str, function: &str| ask(&endpoint, "light", Request::command(function).at(id));

    let lamp = bridge.light_child(2);
    assert_eq!(light_state(command(&lamp, "off").unwrap()).on, Some(false));
    assert_eq!(
        light_state(command(&lamp, "toggle").unwrap()).on,
        Some(true)
    );
    assert_eq!(
        bridge.resource("light", &bridge.light_id(2))["on"]["on"],
        true
    );
    assert_eq!(
        light_state(command(&lamp, "toggle").unwrap()).on,
        Some(false)
    );

    // A lamp that is not on the mesh has no state, so it cannot be toggled
    // into a guess - and it is never reported as "off".
    let missing = bridge.light_child(5);
    assert!(!bridge::reachable(5));
    assert_eq!(
        status_of(&endpoint, "light", &missing).light.unwrap().on,
        None
    );
    let refusal = command(&missing, "toggle").unwrap_err();
    assert_eq!(refusal.code, Error::Rejected);
    assert!(refusal.reason.unwrap().text().contains("not answering"));
    // ...but an explicit on is a decision the person made, and is sent.
    assert_eq!(light_state(command(&missing, "on").unwrap()).on, Some(true));
}

#[test]
fn a_room_takes_one_command_a_second_and_the_next_one_is_refused_without_a_request() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let room = bridge.group_child(0);
    let write = |action| ask(&endpoint, "group", Request::action(action).at(&room));
    bridge.clear_log();

    let state = light_state(write(set_light(None, Some(30), None, None)).unwrap());
    assert_eq!(state.brightness, Some(30));
    // A slider dragged across a room sends one command a second; the bridge
    // drops the rest silently, which leaves the room at a level nobody asked
    // for, so the second one is refused here instead.
    let refusal = write(set_light(None, Some(31), None, None)).unwrap_err();
    assert_eq!(refusal.code, Error::Rejected);
    assert!(refusal.reason.unwrap().text().contains("one room command"));
    assert_eq!(
        bridge
            .requests()
            .iter()
            .filter(|line| line.starts_with("PUT"))
            .count(),
        1,
        "the refused command must not have been sent"
    );

    // A second later it is taken again, and a different room never waited.
    thread::sleep(couch_hue::session::GROUP_INTERVAL);
    assert_eq!(
        light_state(write(set_light(None, Some(32), None, None)).unwrap()).brightness,
        Some(32)
    );
    assert_eq!(
        light_state(
            ask(
                &endpoint,
                "group",
                Request::action(set_light(Some(true), None, None, None)).at(&bridge.group_child(1))
            )
            .unwrap()
        )
        .on,
        Some(true)
    );
}

#[test]
fn a_scene_is_recalled_and_has_nothing_to_report_afterwards() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let scene = bridge.scene_child(0);
    bridge.clear_log();

    // A scene is not a switch: `on` recalls it, and it is answered plainly.
    assert_eq!(
        ask(&endpoint, "scene", Request::command("on").at(&scene)).unwrap(),
        Response::Ok
    );
    assert!(bridge
        .requests()
        .iter()
        .any(|line| line == &format!("PUT /clip/v2/resource/scene/{}", bridge.scene_id(0))));
    // A scene reports no state at all: it is not a lamp.
    let status = status_of(&endpoint, "scene", &scene);
    assert_eq!(status.light, None);
    assert_eq!(status, Status::default());
    // And it cannot be switched off.
    assert_eq!(
        ask(&endpoint, "scene", Request::command("off").at(&scene)).map_err(|failure| failure.code),
        Err(Error::Unsupported)
    );
}

#[test]
fn a_key_the_bridge_stops_accepting_asks_the_person_to_pair_again() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = paired(&bridge, &slot);
    let lamp = bridge.light_child(0);
    assert!(status_of(&endpoint, "light", &lamp).light.is_some());

    // Somebody deleted this remote's entry in the Hue app.
    bridge.revoke();
    let refusal = ask(
        &endpoint,
        "light",
        Request::action(set_light(Some(true), None, None, None)).at(&lamp),
    )
    .unwrap_err();
    assert_eq!(
        refusal.code,
        Error::Unpaired,
        "only pairing again fixes a revoked key"
    );
    let text = refusal.reason.expect("a sentence for the person");
    assert_eq!(text.text(), couch_hue::REPAIR);
    assert!(!text.text().contains(bridge.application_key()));

    // And a read after it says the same thing rather than a stale value.
    let refusal = ask(&endpoint, "light", Request::status().at(&lamp)).unwrap_err();
    assert_eq!(refusal.code, Error::Unpaired);
}

#[test]
fn the_key_and_the_certificate_reach_the_package_only_through_the_credential() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    // The settings are the address and nothing else: no key, no certificate.
    let settings = bridge.settings_value();
    assert_eq!(settings.as_object().unwrap().len(), 1);
    assert!(settings["host"].is_string());
    slot.manifest
        .validate_settings(&settings)
        .expect("the manifest accepts them");

    // A credential built from the bridge's own key and certificate is what
    // makes the connection work, and nothing else would.
    let endpoint = paired(&bridge, &slot);
    assert!(status_of(&endpoint, "light", &bridge.light_child(0))
        .light
        .is_some());

    // The same settings with somebody else's certificate cannot reach it: the
    // pin is the whole trust decision.
    let stranger = FakeBridge::start();
    let wrong = couch_hue::credential::HueCredential::new(
        bridge.application_key(),
        bridge.bridge_id(),
        stranger.certificate().to_vec(),
    );
    let refused = slot.endpoint(
        bridge.settings_value(),
        Some(&wrong.to_credential()),
        PATIENCE,
    );
    let failure = loop {
        match ask(
            &refused,
            "light",
            Request::status().at(&bridge.light_child(0)),
        ) {
            Err(failure) => break failure,
            // A cold cache answers "unknown" once before the first read of
            // the bridge has failed; the failure follows.
            Ok(_) => thread::sleep(Duration::from_millis(200)),
        }
    };
    assert_eq!(failure.code, Error::Transport);

    // A credential that is not this package's shape at all is refused outright.
    let nonsense = Credential::new(serde_json::json!({"token": "whatever"})).unwrap();
    let broken = slot.endpoint(bridge.settings_value(), Some(&nonsense), PATIENCE);
    assert_eq!(
        ask(
            &broken,
            "light",
            Request::status().at(&bridge.light_child(0))
        )
        .map_err(|failure| failure.code),
        Err(Error::Unpaired)
    );
}
