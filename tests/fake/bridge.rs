//! A fake Hue bridge: CLIP v2 over TLS on 127.0.0.1, with a generated
//! household behind it.
//!
//! It exists because the package's whole job is to be careful with a bridge -
//! a pinned certificate, a key it never prints, a cache that must not answer
//! from the network, a listing that must not shift under paging - and none of
//! that can be proved against a real one without touching somebody's lights.
//!
//! What makes it worth the size:
//!
//! - **It is really TLS.** A self-signed certificate whose common name is the
//!   bridge id, as a real bridge's is, served by rustls. The package trusts it
//!   only because the certificate reached it inside a credential; there is no
//!   test-only trust bypass anywhere in this repository, and the package would
//!   refuse any other certificate.
//! - **The household is generated**: 48 lights, 14 rooms, 2 zones, 181 scenes,
//!   with the devices, zigbee links and grouped lights that hold them
//!   together - the shape and the size of the bridge this package was written
//!   against.
//! - **Writes really change it**, and the next read agrees; a wrong key is a
//!   401; an event stream says so afterwards.
//! - **It can misbehave**: `silent()` answers nothing while staying connected,
//!   `close()` stops being a bridge at all.
//! - **It keeps a log** of every request that reached it, which is how a test
//!   proves a status read touched no network.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use couch_hue::credential::HueCredential;
use couch_plugin::testing::FakeDevice;
use couch_sdk::Credential;
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection, StreamOwned,
};
use serde_json::{json, Value};

/// The household, in the proportions of the bridge this package was written
/// against (read-only, on 2026-09-19).
pub const LIGHTS: usize = 48;
pub const ROOMS: usize = 14;
pub const ZONES: usize = 2;
pub const SCENES: usize = 181;
/// Every light, every room's grouped light and every scene. Zones are not
/// children: a zone's grouped light is not listed, only its scenes are.
pub const CHILDREN: usize = LIGHTS + ROOMS + SCENES;

/// The room the safety rules in the hardware script are written around. It is
/// here so that a test can prove the hint reaches the listing under the name
/// a person would recognise.
pub const LIVING_ROOM: &str = "Living room";

pub const ROOM_NAMES: [&str; ROOMS] = [
    LIVING_ROOM,
    "Kitchen",
    "Hallway",
    "Study",
    "Bedroom",
    "Bathroom",
    "Landing",
    "Dining room",
    "Garage",
    "Garden",
    "Nursery",
    "Office",
    "Utility",
    "Loft",
];
pub const ZONE_NAMES: [&str; ZONES] = ["Downstairs", "Upstairs"];

/// How the bridge is behaving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A bridge that answers.
    Normal,
    /// Connected, reading, and answering nothing: the bridge whose Wi-Fi is
    /// there but whose CPU is not. Every open stream stays open.
    Silent,
    /// Gone. The listener is dropped, so a connection is refused rather than
    /// hanging, and every event stream ends.
    Close,
}

// ---------------------------------------------------------------------------
// Identity and the generated household.
// ---------------------------------------------------------------------------

/// A v2 resource id: 36 characters, hyphens where a UUID has them.
fn uuid(tag: u16, index: usize) -> String {
    format!(
        "{:08x}-{:04x}-4000-8000-{:012x}",
        index as u32,
        tag,
        (index as u64) + ((tag as u64) << 32)
    )
}

const LIGHT: u16 = 0x1;
const DEVICE: u16 = 0x2;
const ZIGBEE: u16 = 0x3;
const ROOM: u16 = 0x4;
const ROOM_GROUP: u16 = 0x5;
const SCENE: u16 = 0x6;
const ZONE: u16 = 0x7;
const ZONE_GROUP: u16 = 0x8;

/// Which lamp has which trait, chosen so that every interesting combination is
/// somebody's lamp: one that cannot be dimmed, one that has no colour
/// temperature, one that is not on the mesh at all.
pub fn dimmable(light: usize) -> bool {
    light % 8 != 7
}
pub fn tunable(light: usize) -> bool {
    light % 3 == 0
}
pub fn coloured(light: usize) -> bool {
    light % 5 == 0
}
pub fn reachable(light: usize) -> bool {
    light != 5
}
pub fn light_room(light: usize) -> usize {
    light % ROOMS
}
/// Scenes go round the rooms and then the two zones, so a zone's scenes are
/// listed (with the zone's name as their hint) while the zone itself is not.
pub fn scene_group(scene: usize) -> usize {
    scene % (ROOMS + ZONES)
}

fn scene_name(scene: usize) -> String {
    format!(
        "{} {scene}",
        ["Relax", "Bright", "Nightlight", "Concentrate", "Energize"][scene % 5]
    )
}

/// Every resource the bridge serves, in one array, exactly as
/// `GET /clip/v2/resource` returns it.
fn household() -> Vec<Value> {
    let mut all = Vec::new();
    for index in 0..LIGHTS {
        let device = uuid(DEVICE, index);
        let mut light = json!({
            "id": uuid(LIGHT, index),
            "type": "light",
            "owner": {"rid": device, "rtype": "device"},
            "metadata": {"name": format!("Lamp {index}"), "archetype": "sultan_bulb"},
            "on": {"on": index % 2 == 0},
        });
        if dimmable(index) {
            light["dimming"] = json!({
                "brightness": (10 + (index % 9) * 10) as f64,
                "min_dim_level": 0.2,
            });
        }
        if tunable(index) {
            light["color_temperature"] = json!({
                "mirek": 153 + (index % 10) * 30,
                "mirek_valid": true,
                "mirek_schema": {"mirek_minimum": 153, "mirek_maximum": 500},
            });
        }
        if coloured(index) {
            // Present, and deliberately ignored: this package has no colour.
            light["color"] = json!({"xy": {"x": 0.41, "y": 0.38}});
        }
        all.push(json!({
            "id": device,
            "type": "device",
            "metadata": {"name": format!("Lamp {index}")},
            "services": [
                {"rid": uuid(LIGHT, index), "rtype": "light"},
                {"rid": uuid(ZIGBEE, index), "rtype": "zigbee_connectivity"},
            ],
        }));
        all.push(json!({
            "id": uuid(ZIGBEE, index),
            "type": "zigbee_connectivity",
            "owner": {"rid": device, "rtype": "device"},
            "status": if reachable(index) { "connected" } else { "connectivity_issue" },
        }));
        all.push(light);
    }
    for (index, name) in ROOM_NAMES.iter().enumerate() {
        let children: Vec<Value> = (0..LIGHTS)
            .filter(|light| light_room(*light) == index)
            .map(|light| json!({"rid": uuid(DEVICE, light), "rtype": "device"}))
            .collect();
        all.push(json!({
            "id": uuid(ROOM, index),
            "type": "room",
            "metadata": {"name": name, "archetype": "living_room"},
            "children": children,
            "services": [{"rid": uuid(ROOM_GROUP, index), "rtype": "grouped_light"}],
        }));
        all.push(json!({
            "id": uuid(ROOM_GROUP, index),
            "type": "grouped_light",
            "owner": {"rid": uuid(ROOM, index), "rtype": "room"},
            "on": {"on": index % 2 == 0},
            "dimming": {"brightness": 50.0},
        }));
    }
    for (index, name) in ZONE_NAMES.iter().enumerate() {
        let children: Vec<Value> = (0..LIGHTS)
            .filter(|light| light % ZONES == index)
            .take(6)
            .map(|light| json!({"rid": uuid(LIGHT, light), "rtype": "light"}))
            .collect();
        all.push(json!({
            "id": uuid(ZONE, index),
            "type": "zone",
            "metadata": {"name": name, "archetype": "other"},
            "children": children,
            "services": [{"rid": uuid(ZONE_GROUP, index), "rtype": "grouped_light"}],
        }));
        all.push(json!({
            "id": uuid(ZONE_GROUP, index),
            "type": "grouped_light",
            "owner": {"rid": uuid(ZONE, index), "rtype": "zone"},
            "on": {"on": true},
            "dimming": {"brightness": 70.0},
        }));
    }
    for index in 0..SCENES {
        let group = scene_group(index);
        let (rid, rtype) = if group < ROOMS {
            (uuid(ROOM, group), "room")
        } else {
            (uuid(ZONE, group - ROOMS), "zone")
        };
        all.push(json!({
            "id": uuid(SCENE, index),
            "type": "scene",
            "metadata": {"name": scene_name(index)},
            "group": {"rid": rid, "rtype": rtype},
        }));
    }
    all
}

// ---------------------------------------------------------------------------
// The server.
// ---------------------------------------------------------------------------

struct Shared {
    id: String,
    key: String,
    mode: Mutex<Mode>,
    running: AtomicBool,
    /// Whether the link button has been pressed. A pairing `POST /api` before
    /// that is Hue error 101.
    pressed: AtomicBool,
    /// Whether pairing is refused outright, whatever the button says.
    refusing: AtomicBool,
    /// Whether the key it issued still works. A bridge that was factory reset,
    /// or whose entry somebody deleted in the Hue app.
    revoked: AtomicBool,
    log: Mutex<Vec<String>>,
    resources: Mutex<Vec<Value>>,
    listeners: Mutex<Vec<Sender<String>>>,
}

impl Shared {
    fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode")
    }
    fn record(&self, line: String) {
        self.log.lock().expect("log").push(line);
    }
    fn find(&self, kind: &str, id: &str) -> Option<usize> {
        self.resources
            .lock()
            .expect("resources")
            .iter()
            .position(|value| value["type"] == kind && value["id"] == id)
    }
    /// Tell every open event stream that something changed. The body is the
    /// shape a bridge sends; the package only reads the event type from it and
    /// re-reads the whole resource tree.
    fn emit(&self, kind: &str, id: &str) {
        let event = format!(
            "id: {}:0\ndata: {}\n\n",
            id,
            json!([{
                "id": uuid(0xf, 1),
                "type": "update",
                "creationtime": "2026-09-20T12:00:00Z",
                "data": [{"id": id, "type": kind}],
            }])
        );
        self.listeners
            .lock()
            .expect("listeners")
            .retain(|listener| listener.send(event.clone()).is_ok());
    }
}

/// A fake Hue bridge, listening until it is dropped.
pub struct FakeBridge {
    address: SocketAddr,
    certificate: Vec<u8>,
    shared: Arc<Shared>,
}

impl FakeBridge {
    /// One bridge, listening on a port the operating system chooses, with the
    /// link button not yet pressed.
    pub fn start() -> Self {
        Self::with_id(&format!(
            "001788FFFE{:06X}",
            std::process::id() & 0x00ff_ffff
        ))
    }

    pub fn with_id(id: &str) -> Self {
        let (certificate, private) = self_signed(id);
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(certificate.clone())],
                    PrivatePkcs8KeyDer::from(private).into(),
                )
                .expect("a self-signed certificate and its key");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener.local_addr().expect("the chosen port");
        listener
            .set_nonblocking(true)
            .expect("a listener that can be stopped");
        let shared = Arc::new(Shared {
            id: id.to_string(),
            key: format!("fakeApplicationKey{:04x}", address.port()),
            mode: Mutex::new(Mode::Normal),
            running: AtomicBool::new(true),
            pressed: AtomicBool::new(false),
            refusing: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            log: Mutex::new(Vec::new()),
            resources: Mutex::new(household()),
            listeners: Mutex::new(Vec::new()),
        });
        let accepting = shared.clone();
        let config = Arc::new(config);
        thread::Builder::new()
            .name("fake-hue-bridge".into())
            .spawn(move || accept(listener, accepting, config))
            .expect("the bridge's accept thread");
        Self {
            address,
            certificate,
            shared,
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
    /// The settings a person would type: the bridge's address, and nothing
    /// else. The key and the certificate are never settings.
    pub fn settings_value(&self) -> Value {
        json!({"host": self.address.to_string()})
    }
    pub fn bridge_id(&self) -> &str {
        &self.shared.id
    }
    /// The application key the bridge issues and accepts. A test builds a
    /// credential from it; the package is never told it any other way.
    pub fn application_key(&self) -> &str {
        &self.shared.key
    }
    /// The exact certificate this bridge presents, in DER.
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    /// What Couch would be holding after a successful pairing with this
    /// bridge. The certificate reaches the package only through this.
    pub fn hue_credential(&self) -> HueCredential {
        HueCredential::new(
            self.application_key(),
            self.bridge_id(),
            self.certificate.clone(),
        )
    }
    pub fn credential(&self) -> Credential {
        self.hue_credential().to_credential()
    }

    /// The link button. Until it is pressed, a pairing attempt is Hue error
    /// 101.
    pub fn press(&self) {
        self.shared.pressed.store(true, Ordering::SeqCst);
    }
    /// A bridge that refuses to pair at all, button or no button.
    pub fn refuse(&self) {
        self.shared.refusing.store(true, Ordering::SeqCst);
    }
    /// Stop accepting the key it issued: a bridge that was reset, or whose
    /// entry for Couch somebody deleted in the Hue app.
    pub fn revoke(&self) {
        self.shared.revoked.store(true, Ordering::SeqCst);
    }

    pub fn mode(&self) -> Mode {
        self.shared.mode()
    }
    /// Stop answering, without dropping anything. An open event stream stays
    /// open and silent, which is how a bridge looks while its cache is still
    /// believed.
    pub fn silent(&self) {
        *self.shared.mode.lock().expect("mode") = Mode::Silent;
    }
    pub fn speak(&self) {
        *self.shared.mode.lock().expect("mode") = Mode::Normal;
    }
    /// Stop being a bridge: the listener goes, so a connection is refused, and
    /// every event stream ends. One way.
    pub fn close(&self) {
        *self.shared.mode.lock().expect("mode") = Mode::Close;
        self.shared.listeners.lock().expect("listeners").clear();
        // The accept thread notices within a poll and drops the listener.
        thread::sleep(Duration::from_millis(30));
    }

    /// Every request that has reached the bridge, oldest first, as
    /// `"METHOD path"`.
    pub fn requests(&self) -> Vec<String> {
        self.shared.log.lock().expect("log").clone()
    }
    pub fn clear_log(&self) {
        self.shared.log.lock().expect("log").clear();
    }
    /// How many event streams are open right now.
    pub fn streams(&self) -> usize {
        self.shared.listeners.lock().expect("listeners").len()
    }
    /// Say that something changed, without anything having changed. Used to
    /// make a listing race an event.
    pub fn nudge(&self) {
        self.shared.emit("light", &self.light_id(0));
    }

    /// One resource as the bridge currently holds it.
    pub fn resource(&self, kind: &str, id: &str) -> Value {
        let index = self
            .shared
            .find(kind, id)
            .unwrap_or_else(|| panic!("no {kind} {id}"));
        self.shared.resources.lock().expect("resources")[index].clone()
    }

    pub fn light_id(&self, index: usize) -> String {
        uuid(LIGHT, index)
    }
    pub fn room_group_id(&self, index: usize) -> String {
        uuid(ROOM_GROUP, index)
    }
    pub fn zone_group_id(&self, index: usize) -> String {
        uuid(ZONE_GROUP, index)
    }
    pub fn scene_id(&self, index: usize) -> String {
        uuid(SCENE, index)
    }
    /// The child id the package gives one of this bridge's lamps, rooms or
    /// scenes.
    pub fn light_child(&self, index: usize) -> String {
        self.light_id(index)
    }
    pub fn group_child(&self, index: usize) -> String {
        format!("room/{}", self.room_group_id(index))
    }
    pub fn scene_child(&self, index: usize) -> String {
        format!("scene/{}", self.scene_id(index))
    }
    /// The first lamp of a room, by the rule the generator used.
    pub fn light_in_room(&self, room: usize) -> usize {
        (0..LIGHTS)
            .find(|light| light_room(*light) == room)
            .expect("every room has a lamp")
    }
}

impl Drop for FakeBridge {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.listeners.lock().expect("listeners").clear();
    }
}

impl FakeDevice for FakeBridge {
    fn settings(&self) -> Value {
        self.settings_value()
    }
    fn requests(&self) -> Vec<String> {
        self.shared.log.lock().expect("log").clone()
    }
}

fn self_signed(id: &str) -> (Vec<u8>, Vec<u8>) {
    // The common name is the bridge id, as a real bridge's certificate has it:
    // the certificate never names the address it is reached at, which is why
    // Couch pins the certificate itself rather than trusting a name.
    let mut parameters =
        rcgen::CertificateParams::new(vec![id.to_string()]).expect("certificate parameters");
    parameters.distinguished_name = rcgen::DistinguishedName::new();
    parameters
        .distinguished_name
        .push(rcgen::DnType::CommonName, id);
    let key = rcgen::KeyPair::generate().expect("a key pair");
    let certificate = parameters.self_signed(&key).expect("a self-signed bridge");
    (certificate.der().to_vec(), key.serialize_der())
}

fn accept(listener: TcpListener, shared: Arc<Shared>, config: Arc<ServerConfig>) {
    loop {
        if !shared.running.load(Ordering::SeqCst) || shared.mode() == Mode::Close {
            return; // dropping the listener refuses every later connection
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = shared.clone();
                let config = config.clone();
                let _ = thread::Builder::new()
                    .name("fake-hue-connection".into())
                    .spawn(move || connection(stream, shared, config));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

type Tls = StreamOwned<ServerConnection, TcpStream>;

fn connection(stream: TcpStream, shared: Arc<Shared>, config: Arc<ServerConfig>) {
    // On the BSDs, and so on macOS, an accepted socket inherits the
    // listener's non-blocking flag. The listener has to be non-blocking to be
    // stoppable; this connection must not be.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let Ok(session) = ServerConnection::new(config) else {
        return;
    };
    let mut tls = StreamOwned::new(session, stream);
    let Some(request) = read_request(&mut tls) else {
        return;
    };
    shared.record(format!("{} {}", request.method, request.path));
    if shared.mode() == Mode::Silent {
        // Connected, read, and never answered.
        while shared.running.load(Ordering::SeqCst) && shared.mode() == Mode::Silent {
            thread::sleep(Duration::from_millis(20));
        }
        return;
    }
    if shared.mode() == Mode::Close {
        return;
    }
    route(&shared, &request, &mut tls);
}

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(tls: &mut Tls) -> Option<Request> {
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if tls.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8(head).ok()?;
    let mut lines = text.lines();
    let mut start = lines.next()?.split_whitespace();
    let method = start.next()?.to_string();
    let path = start.next()?.to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length.min(1024 * 1024)];
    if !body.is_empty() {
        tls.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn send(tls: &mut Tls, status: u16, body: &Value) {
    let text = body.to_string();
    let _ = write!(
        tls,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        if status == 200 { "OK" } else { "Error" },
        text.len(),
    );
    let _ = tls.flush();
}

fn unauthorised(tls: &mut Tls) {
    send(
        tls,
        401,
        &json!({"errors": [{"description": "unauthorized user"}]}),
    );
}

fn route(shared: &Arc<Shared>, request: &Request, tls: &mut Tls) {
    let key = request
        .headers
        .get("hue-application-key")
        .cloned()
        .unwrap_or_default();
    let authorised = key == shared.key && !shared.revoked.load(Ordering::SeqCst);
    match (request.method.as_str(), request.path.as_str()) {
        // Pairing: the one unauthenticated route a bridge has.
        ("POST", "/api") => {
            if shared.refusing.load(Ordering::SeqCst) {
                send(
                    tls,
                    200,
                    &json!([{"error": {"type": 7, "address": "/devicetype", "description": "invalid value"}}]),
                );
            } else if shared.pressed.load(Ordering::SeqCst) {
                send(tls, 200, &json!([{"success": {"username": shared.key}}]));
            } else {
                send(
                    tls,
                    200,
                    &json!([{"error": {"type": 101, "address": "/", "description": "link button not pressed"}}]),
                );
            }
        }
        ("GET", "/api/0/config") => send(
            tls,
            200,
            &json!({
                "name": "Fake bridge",
                "bridgeid": shared.id,
                "swversion": "1962097030",
                "apiversion": "1.62.0",
                "modelid": "BSB002",
            }),
        ),
        ("GET", "/clip/v2/resource") if !authorised => unauthorised(tls),
        ("GET", "/clip/v2/resource") => {
            let data = shared.resources.lock().expect("resources").clone();
            send(tls, 200, &json!({"errors": [], "data": data}));
        }
        ("GET", "/eventstream/clip/v2") if !authorised => unauthorised(tls),
        ("GET", "/eventstream/clip/v2") => stream_events(shared, tls),
        ("PUT", path) if path.starts_with("/clip/v2/resource/") => {
            if !authorised {
                return unauthorised(tls);
            }
            write_resource(shared, path, &request.body, tls);
        }
        _ => send(
            tls,
            404,
            &json!({"errors": [{"description": "resource not found"}]}),
        ),
    }
}

fn write_resource(shared: &Arc<Shared>, path: &str, body: &[u8], tls: &mut Tls) {
    let rest = &path["/clip/v2/resource/".len()..];
    let Some((kind, id)) = rest.split_once('/') else {
        return send(
            tls,
            404,
            &json!({"errors": [{"description": "resource not found"}]}),
        );
    };
    if !matches!(kind, "light" | "grouped_light" | "scene") {
        return send(
            tls,
            404,
            &json!({"errors": [{"description": "resource not found"}]}),
        );
    }
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return send(
            tls,
            400,
            &json!({"errors": [{"description": "body is not json"}]}),
        );
    };
    let Some(index) = shared.find(kind, id) else {
        return send(
            tls,
            404,
            &json!({"errors": [{"description": "resource not found"}]}),
        );
    };
    if kind == "scene" {
        if body["recall"]["action"] != "active" {
            return send(
                tls,
                200,
                &json!({"errors": [{"description": "scene can only be recalled"}], "data": []}),
            );
        }
        shared.emit("scene", id);
        return send(
            tls,
            200,
            &json!({"errors": [], "data": [{"rid": id, "rtype": kind}]}),
        );
    }
    {
        let mut resources = shared.resources.lock().expect("resources");
        let target = &mut resources[index];
        if body.get("dimming").is_some() && target.get("dimming").is_none() {
            // What a real bridge says when a lamp has no dimming service: HTTP
            // 200, and the failure inside it.
            drop(resources);
            return send(
                tls,
                200,
                &json!({"errors": [{"description": "device does not support dimming"}], "data": []}),
            );
        }
        if let Some(on) = body["on"]["on"].as_bool() {
            target["on"] = json!({"on": on});
        }
        if let Some(brightness) = body["dimming"]["brightness"].as_f64() {
            target["dimming"]["brightness"] = json!(brightness);
        }
        if let Some(mirek) = body["color_temperature"]["mirek"].as_u64() {
            if target.get("color_temperature").is_none() {
                drop(resources);
                return send(
                    tls,
                    200,
                    &json!({"errors": [{"description": "device does not support color temperature"}], "data": []}),
                );
            }
            target["color_temperature"]["mirek"] = json!(mirek);
            target["color_temperature"]["mirek_valid"] = json!(true);
        }
    }
    shared.emit(kind, id);
    send(
        tls,
        200,
        &json!({"errors": [], "data": [{"rid": id, "rtype": kind}]}),
    );
}

fn stream_events(shared: &Arc<Shared>, tls: &mut Tls) {
    let (sender, receiver): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
    shared.listeners.lock().expect("listeners").push(sender);
    let written = write!(
        tls,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n: hi\n\n"
    );
    if written.is_err() || tls.flush().is_err() {
        return;
    }
    loop {
        if !shared.running.load(Ordering::SeqCst) || shared.mode() == Mode::Close {
            return;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                if tls.write_all(event.as_bytes()).is_err() || tls.flush().is_err() {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if shared.mode() == Mode::Silent {
                    continue; // connected, and saying nothing
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
