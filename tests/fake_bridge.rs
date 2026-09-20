//! What the fake bridge is, proved before anything is proved against it.
//!
//! A fixture that quietly does not do what it claims turns every test above it
//! into a test of nothing, so its five claims are checked here: it is really
//! TLS and the certificate is really pinned, the household it generates is the
//! size and shape it says, a wrong key is refused, a write changes it and the
//! next read agrees, and the event stream says so afterwards.

mod fake;

use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use couch_hue::{Command, Error, Hue};
use fake::bridge::{self, FakeBridge};

fn client(bridge: &FakeBridge) -> Hue {
    Hue::new(
        &bridge.address().to_string(),
        bridge.application_key(),
        &bridge.certificate(),
    )
    .expect("a client for the fake bridge")
}

#[test]
fn the_household_is_the_size_and_shape_a_real_bridge_has() {
    let bridge = FakeBridge::start();
    let resources = client(&bridge).resources().expect("a full read");
    let counted = |kind: &str| {
        resources
            .iter()
            .filter(|resource| resource.resource_kind == kind)
            .count()
    };
    // 48 lamps, 14 rooms and 181 scenes: the bridge this package was written
    // against, read-only, on 2026-09-19.
    assert_eq!(counted("light"), bridge::LIGHTS);
    assert_eq!(counted("room"), bridge::ROOMS);
    assert_eq!(counted("scene"), bridge::SCENES);
    assert_eq!(resources.len(), bridge::CHILDREN);

    // A zone has a grouped light of its own and is deliberately not a room.
    let zones = bridge.resource("grouped_light", &bridge.zone_group_id(0));
    assert_eq!(zones["owner"]["rtype"], "zone");

    // One lamp is off the mesh, so its state is unknown rather than off.
    let unreachable = resources
        .iter()
        .find(|resource| resource.state.entity_id == bridge.light_id(5))
        .expect("the lamp that is not on the mesh");
    assert_eq!(unreachable.state.on, None);
    let lit = resources
        .iter()
        .find(|resource| resource.state.entity_id == bridge.light_id(0))
        .expect("a lamp that is");
    assert_eq!(lit.state.on, Some(true));
    assert!(lit.state.dimmable);
}

#[test]
fn a_wrong_key_is_refused_and_a_wrong_certificate_never_gets_that_far() {
    let bridge = FakeBridge::start();
    let wrong_key = Hue::new(
        &bridge.address().to_string(),
        "notTheApplicationKey",
        &bridge.certificate(),
    )
    .expect("a client with the wrong key");
    assert_eq!(wrong_key.lights().unwrap_err(), Error::Authentication);
    assert_eq!(
        wrong_key.set_power(&bridge.light_id(0), true).unwrap_err(),
        Error::Authentication
    );

    // Somebody else's certificate. The pin is the whole trust decision, so
    // this fails in the handshake and never reaches a route.
    let stranger = FakeBridge::start();
    let wrong_certificate = Hue::new(
        &bridge.address().to_string(),
        bridge.application_key(),
        &stranger.certificate(),
    )
    .expect("a client with the wrong certificate");
    bridge.clear_log();
    assert_eq!(wrong_certificate.lights().unwrap_err(), Error::Transport);
    assert!(
        bridge.requests().is_empty(),
        "a refused handshake never reaches a route: {:?}",
        bridge.requests()
    );
}

#[test]
fn a_write_changes_the_bridge_and_the_next_read_agrees() {
    let bridge = FakeBridge::start();
    let hue = client(&bridge);
    let lamp = bridge.light_id(0);
    assert_eq!(bridge.resource("light", &lamp)["on"]["on"], true);
    hue.set_power(&lamp, false).expect("a write");
    assert_eq!(bridge.resource("light", &lamp)["on"]["on"], false);
    assert_eq!(
        hue.control_state(&lamp).expect("a read").on,
        Some(false),
        "the next read has to agree with the write"
    );

    hue.command(&lamp, Command::Brightness(37)).expect("a dim");
    assert_eq!(
        bridge.resource("light", &lamp)["dimming"]["brightness"],
        37.0
    );

    // A lamp with no dimming service answers the way a real bridge does: HTTP
    // 200 with the failure inside it.
    let plain = bridge.light_id(7);
    assert!(!bridge::dimmable(7));
    assert_eq!(
        hue.command(&plain, Command::Brightness(40)).unwrap_err(),
        Error::Brightness,
        "the client refuses it before the bridge has to"
    );

    // A room is the grouped light behind it, and a scene is only recalled.
    let room = bridge.room_group_id(0);
    hue.set_power(&format!("room:{room}"), false)
        .expect("a room write");
    assert_eq!(bridge.resource("grouped_light", &room)["on"]["on"], false);
    hue.recall_scene(&bridge.scene_id(0)).expect("a recall");

    let log = bridge.requests();
    assert!(log.contains(&format!("PUT /clip/v2/resource/light/{lamp}")));
    assert!(log.contains(&format!("PUT /clip/v2/resource/grouped_light/{room}")));
    assert!(log.contains(&format!(
        "PUT /clip/v2/resource/scene/{}",
        bridge.scene_id(0)
    )));
}

#[test]
fn the_event_stream_says_so_after_a_write() {
    let bridge = FakeBridge::start();
    let mut stream = open_stream(&bridge);
    assert_eq!(bridge.streams(), 1, "the bridge holds the open stream");

    client(&bridge)
        .set_power(&bridge.light_id(1), true)
        .expect("a write");
    let event = read_event(&mut stream);
    assert!(event.contains("\"type\":\"update\""), "{event}");
    assert!(event.contains(&bridge.light_id(1)), "{event}");
}

#[test]
fn a_silent_bridge_answers_nothing_and_a_closed_one_refuses_the_connection() {
    let bridge = FakeBridge::start();
    let hue = client(&bridge);
    hue.lights().expect("a bridge that answers");

    bridge.silent();
    let started = Instant::now();
    assert!(hue.lights().is_err(), "a silent bridge cannot answer");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "and it must fail on the client's own deadline"
    );
    assert!(
        bridge
            .requests()
            .iter()
            .filter(|line| *line == "GET /clip/v2/resource")
            .count()
            >= 2,
        "a silent bridge still reads what it is sent"
    );

    bridge.speak();
    hue.lights().expect("a bridge that answers again");

    bridge.close();
    let started = Instant::now();
    assert_eq!(hue.lights().unwrap_err(), Error::Transport);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a closed port is refused, not waited for"
    );
}

// ---------------------------------------------------------------------------
// One pinned TLS connection, by hand, so that the event stream and the pin are
// checked by something other than the code under test.
// ---------------------------------------------------------------------------

struct PinnedStream {
    session: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

fn open_stream(bridge: &FakeBridge) -> PinnedStream {
    let pin = Arc::new(couch_sdk::tls::Pin::new(
        Arc::new(Mutex::new(bridge.certificate())),
        "Hue bridge certificate changed; pair again",
    ));
    let config = couch_sdk::tls::pinned_client_config(pin).expect("a pinned client");
    let socket = TcpStream::connect(bridge.address()).expect("the fake bridge's port");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("a deadline");
    let name = rustls::pki_types::ServerName::try_from(bridge.bridge_id().to_string())
        .expect("the bridge id as a name");
    let session =
        rustls::ClientConnection::new(Arc::new(config), name).expect("a TLS client session");
    let mut session = rustls::StreamOwned::new(session, socket);
    write!(
        session,
        "GET /eventstream/clip/v2 HTTP/1.1\r\nHost: {}\r\nhue-application-key: {}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        bridge.bridge_id(),
        bridge.application_key(),
    )
    .expect("the request");
    session.flush().expect("the request");
    let mut stream = PinnedStream { session };
    let head = read_until(&mut stream, "\r\n\r\n");
    assert!(head.contains("text/event-stream"), "{head}");
    stream
}

fn read_until(stream: &mut PinnedStream, terminator: &str) -> String {
    let mut text = String::new();
    loop {
        let mut byte = [0u8; 1];
        let read = stream.session.read(&mut byte).expect("the bridge's answer");
        assert!(read > 0, "the stream ended before {terminator:?}");
        text.push(byte[0] as char);
        if text.ends_with(terminator) {
            return text;
        }
        assert!(text.len() < 64 * 1024, "an event that never ends");
    }
}

/// The next event that carries data. A stream also carries comments - the
/// bridge's own keep-alive - and they are not events.
fn read_event(stream: &mut PinnedStream) -> String {
    for _ in 0..16 {
        let text = read_until(stream, "\n\n");
        let data: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: ").or(line.strip_prefix("data:")))
            .collect();
        if !data.is_empty() {
            return data;
        }
    }
    panic!("the stream carried no event");
}
