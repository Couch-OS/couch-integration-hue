//! Pairing, against the fake bridge and through the real package.
//!
//! The first case is the shared admission harness,
//! `couch_plugin::testing_v3::pairing`, which drives a real subprocess
//! through all four ways a conversation can end and asserts every bound a
//! step carries. The rest are the things a Hue bridge does that the harness
//! cannot express: the button polled until it is pressed, a bridge that
//! refuses, one that is not there, the certificate pinned by the first
//! handshake and refused when it changes, and the whole path from an address
//! a person typed to 243 children listed.
//!
//! Everything here talks to `tests/fake/bridge.rs` on 127.0.0.1. No real
//! bridge is contacted, and nothing here discovers anything.

mod fake;

use std::{
    thread,
    time::{Duration, Instant},
};

use couch_hue::{
    adapter::HueSettings,
    pairing::{HueFlow, PAIR_BUDGET},
};
use couch_plugin::{
    list_children,
    testing::{Adapter, FakeDevice},
    testing_v3::{self, PairingCase, PairingScenario},
    Credential, PairFailure, PairFlow, PairStep, Request,
};
use fake::{
    bridge::{self, FakeBridge},
    slot,
};
use serde_json::Value;

const PATIENCE: Duration = Duration::from_secs(8);

fn adapter() -> Adapter<'static> {
    Adapter {
        binary: std::path::Path::new(env!("CARGO_BIN_EXE_couch-plugin-hue")),
        manifest_json: include_str!("../plugin.json"),
    }
}

fn settings(bridge: &FakeBridge) -> HueSettings {
    HueSettings {
        host: bridge.address().to_string(),
    }
}

/// Poll a flow to its end, the way the host would but without the waiting.
fn converse(flow: &mut dyn PairFlow, limit: usize) -> PairStep {
    for _ in 0..limit {
        let step = flow.step(None).expect("a flow answers every step");
        if step.is_final() {
            return step;
        }
    }
    panic!("the conversation never ended");
}

fn waiting(step: &PairStep) -> &str {
    match step {
        PairStep::Waiting {
            prompt,
            poll_after_ms,
        } => {
            assert_eq!(*poll_after_ms, 2000, "the poll interval moved");
            prompt.message().expect("a line under Couch's headline")
        }
        other => panic!("expected a step that waits, got {other:?}"),
    }
}

fn failure(step: &PairStep) -> (PairFailure, String) {
    match step {
        PairStep::Failed { reason, message } => (*reason, message.clone().unwrap_or_default()),
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn done(step: PairStep) -> (Credential, Value, String) {
    match step {
        PairStep::Done {
            credential,
            settings,
            summary,
        } => (
            credential,
            settings.expect("a Hue pairing corrects the address it reached"),
            summary,
        ),
        other => panic!("expected a key, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The shared admission harness.
// ---------------------------------------------------------------------------

/// Every scenario `couch_plugin::testing_v3::pairing` checks, against a real
/// subprocess: one conversation that works, one refused, one that runs out of
/// time, one the person closes, the bounds of every step on the way, and the
/// one thing a key must never do - come back out in a later answer.
///
/// **It takes about two minutes**, and all of that is the `TimedOut`
/// scenario, which is not padding: the only honest way to prove a package
/// gives up on its own budget is to let one give up on its own budget. The
/// bridge in that scenario is reachable and answers nothing, so every poll
/// costs this package's four second step budget, and the conversation ends
/// after the thirtieth or so, past [`PAIR_BUDGET`]. The harness allows
/// forty-one polls, so there is room; it also refuses to skip a scenario, so
/// there is no cheaper way through it.
#[test]
fn every_scenario_the_shared_admission_harness_checks() {
    testing_v3::pairing(
        adapter(),
        PairingCase {
            device: |scenario| {
                let bridge = FakeBridge::start();
                match scenario {
                    // Somebody is at the bridge with a finger on the button.
                    PairingScenario::Paired => bridge.press(),
                    // A bridge that will not link with this remote at all.
                    PairingScenario::Refused => bridge.refuse(),
                    // Nobody presses it, and the dialog is closed instead.
                    PairingScenario::Cancelled => (),
                    // There, reachable, and answering nothing: every poll
                    // costs a step budget until this package's own clock
                    // runs out.
                    PairingScenario::TimedOut => bridge.silent(),
                }
                Some(Box::new(bridge) as Box<dyn FakeDevice>)
            },
            settings: |device, _| device.settings(),
            // A Hue bridge never asks anybody to type anything.
            code: "unused",
            // Something only a paired package can answer, and with no child
            // named: `request_full` sends no kind, and a listing needs none.
            after: Request::children(None),
        },
    );
}

// ---------------------------------------------------------------------------
// What a Hue bridge does.
// ---------------------------------------------------------------------------

#[test]
fn the_button_is_polled_until_somebody_presses_it() {
    let bridge = FakeBridge::start();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow for a reachable address");

    // Error 101, as often as it takes. Each one asks Couch to come back.
    for _ in 0..5 {
        let step = flow.step(None).expect("a flow answers");
        assert_eq!(
            waiting(&step),
            "Press the round button on top of the Hue Bridge"
        );
    }
    assert_eq!(
        bridge
            .requests()
            .iter()
            .filter(|line| *line == "POST /api")
            .count(),
        5,
        "one request per step, and no more"
    );

    bridge.press();
    let (credential, corrected, summary) = done(flow.step(None).expect("a flow answers"));

    // The key, the bridge id and the certificate the first handshake pinned.
    let held = couch_hue::credential::HueCredential::parse(&credential).expect("a Hue credential");
    assert_eq!(held.application_key, bridge.application_key());
    assert_eq!(held.bridge_id, bridge.bridge_id());
    assert_eq!(held.certificate, bridge.certificate());
    assert_eq!(flow.certificate(), bridge.certificate());

    // The address as the package reached it, which is what Couch saves.
    assert_eq!(
        corrected["host"],
        Value::String(format!("https://{}", bridge.address()))
    );
    // One line naming the bridge, by its last six characters and nothing more.
    let tail: String = bridge.bridge_id().chars().rev().take(6).collect::<Vec<_>>()[..]
        .iter()
        .rev()
        .collect();
    assert_eq!(summary, format!("Paired with Hue bridge ...{tail}"));

    // The key was proved before it was handed over: the bridge id was read
    // and one authenticated read was made.
    let log = bridge.requests();
    assert!(log.contains(&"GET /api/0/config".to_string()));
    assert!(log.contains(&"GET /clip/v2/resource".to_string()));
}

#[test]
fn nothing_a_conversation_says_out_loud_carries_the_key_or_the_certificate() {
    let bridge = FakeBridge::start();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow");
    let mut said = vec![waiting(&flow.step(None).expect("a flow answers")).to_string()];
    bridge.press();
    let (credential, _, summary) = done(flow.step(None).expect("a flow answers"));
    said.push(summary);

    // Everything else this flow can say, collected from the other cases.
    let refusing = FakeBridge::start();
    refusing.refuse();
    let mut refused = HueFlow::new(&settings(&refusing)).expect("a flow");
    said.push(failure(&converse(&mut refused, 4)).1);

    // A run of eight characters is enough to be worth something to whoever
    // reads a log; a person telling two bridges apart needs six.
    for value in credential.get().values().filter_map(Value::as_str) {
        let bytes = value.as_bytes();
        if bytes.len() < 8 {
            continue;
        }
        for run in bytes.windows(8) {
            let run = std::str::from_utf8(run).expect("ascii");
            for line in &said {
                assert!(!line.contains(run), "{line:?} carries {run:?}");
            }
        }
    }
    assert!(said.iter().all(|line| !line.is_empty()));
}

#[test]
fn a_bridge_that_will_not_link_ends_the_conversation_at_once() {
    let bridge = FakeBridge::start();
    bridge.refuse();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow");
    let (reason, message) = failure(&converse(&mut flow, 4));
    assert_eq!(reason, PairFailure::Refused);
    assert_eq!(message, "The Hue bridge would not link with Couch");
    assert_eq!(
        bridge
            .requests()
            .iter()
            .filter(|line| *line == "POST /api")
            .count(),
        1,
        "a refusal is not retried"
    );
}

#[test]
fn an_address_with_no_bridge_behind_it_is_unreachable() {
    // A port that was a bridge and is not any more: a loopback port nothing
    // is listening on. Never a LAN address.
    let bridge = FakeBridge::start();
    bridge.close();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow");
    let started = Instant::now();
    let (reason, message) = failure(&converse(&mut flow, 4));
    assert_eq!(reason, PairFailure::Unreachable);
    assert_eq!(message, "No Hue bridge answered at that address");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a closed port is refused, not waited for"
    );
}

#[test]
fn a_conversation_gives_up_on_its_own_budget() {
    let bridge = FakeBridge::start();
    // The shipped budget is 115 seconds, just inside the 120 the manifest
    // gives the dialog. This is the same rule with a shorter clock, because
    // proving it with the shipped one takes two minutes (the admission
    // harness above pays that price once).
    assert_eq!(PAIR_BUDGET, Duration::from_secs(115));
    let mut flow = HueFlow::with_budget(&settings(&bridge), Duration::from_millis(200))
        .expect("a flow with its own clock");
    assert_eq!(
        waiting(&flow.step(None).expect("a flow answers")),
        "Press the round button on top of the Hue Bridge"
    );
    thread::sleep(Duration::from_millis(250));
    let (reason, message) = failure(&flow.step(None).expect("a flow answers"));
    assert_eq!(reason, PairFailure::TimedOut);
    assert!(message.contains("not linked in time"), "{message}");
    // A conversation that has run out costs the bridge nothing more.
    let sent = bridge.requests().len();
    assert!(flow.step(None).expect("a flow answers").is_final());
    assert_eq!(bridge.requests().len(), sent);
}

#[test]
fn the_first_handshake_pins_the_certificate_and_a_later_one_fails_closed() {
    let bridge = FakeBridge::start();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow");
    assert!(
        flow.certificate().is_empty(),
        "nothing is pinned before anything is spoken to"
    );
    waiting(&flow.step(None).expect("a flow answers"));
    assert_eq!(
        flow.certificate(),
        bridge.certificate(),
        "the first handshake is what pins it"
    );

    // Something else answers on that port from now on. The certificate is
    // the whole trust decision, so the conversation ends rather than
    // carrying on with a bridge that is not the one it started with.
    let pinned = bridge.certificate();
    bridge.rotate_certificate();
    assert_ne!(bridge.certificate(), pinned);
    let (reason, message) = failure(&flow.step(None).expect("a flow answers"));
    assert_eq!(reason, PairFailure::Refused);
    assert_eq!(
        message,
        "That bridge presented a different certificate. Start again"
    );
    // ...and the key a conversation like that would have produced is not one
    // Couch would be able to use afterwards either.
    let mut fresh = HueFlow::new(&settings(&bridge)).expect("a flow");
    bridge.press();
    let (credential, _, _) = done(converse(&mut fresh, 4));
    let held = couch_hue::credential::HueCredential::parse(&credential).unwrap();
    assert_eq!(held.certificate, bridge.certificate());
    assert_ne!(held.certificate, pinned);
}

#[test]
fn a_cancelled_conversation_leaves_nothing_and_the_bridge_is_never_told() {
    let bridge = FakeBridge::start();
    let mut flow = HueFlow::new(&settings(&bridge)).expect("a flow");
    waiting(&flow.step(None).expect("a flow answers"));
    let sent = bridge.requests().len();
    flow.cancel();
    assert!(
        flow.certificate().is_empty(),
        "a cancelled flow forgets the certificate it pinned"
    );
    // There is nothing to tell a Hue bridge: it was never told this started.
    assert_eq!(bridge.requests().len(), sent);
}

// ---------------------------------------------------------------------------
// The whole path.
// ---------------------------------------------------------------------------

#[test]
fn pairing_through_the_package_then_configuring_it_lists_the_whole_bridge() {
    let bridge = FakeBridge::start();
    bridge.press();
    let slot = slot::shared();
    let mut host = slot.host(PATIENCE);

    // Pairing, through the real subprocess, exactly as the daemon does it.
    let (session, step) = host
        .pair_start(bridge.settings_value(), None)
        .expect("a package that pairs answers a start");
    assert!(couch_sdk::valid_session(&session));
    let (credential, corrected, summary) = done(step);
    assert!(summary.starts_with("Paired with Hue bridge ..."));
    assert_eq!(host.pair_session(), None, "the conversation is over");
    assert!(host.is_alive(), "pairing cost the package its process");
    drop(host);

    // ...and the key that came out of it opens the bridge.
    let endpoint = slot.endpoint(corrected, Some(&credential), PATIENCE);
    let children =
        list_children(&mut |request| endpoint.request_detailed(request)).expect("a full listing");
    assert_eq!(children.len(), bridge::CHILDREN);
    assert_eq!(children.len(), 243);
    assert_eq!(
        children
            .iter()
            .filter(|child| child.kind == "light")
            .count(),
        bridge::LIGHTS
    );

    // A second pairing of the same bridge is a second key, and works: Hue
    // issues a new one every time the button is pressed, and this package
    // never asks it to reuse one.
    let mut again = slot.host(PATIENCE);
    let (_, step) = again
        .pair_start(bridge.settings_value(), Some(&credential))
        .expect("a re-pair with the key Couch already holds");
    let (second, _, _) = done(step);
    assert!(
        couch_hue::credential::HueCredential::parse(&second).is_ok(),
        "a re-pair produces a credential of the same shape"
    );
}
