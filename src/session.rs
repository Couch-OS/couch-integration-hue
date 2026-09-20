//! One credential-scoped session with one bridge: a cache, the event stream
//! that keeps it current, and the writes that go out past it.
//!
//! The shape of this is decided by one rule: **a status read may not touch the
//! network.** Couch asks a packaged connection for a child's state on every
//! round of a panel that is open, and a bridge that takes a second to answer
//! would make the panel take a second per lamp. So the bridge is read by
//! background threads - an event stream that says when something changed, and
//! a poll that recovers when the stream is down - and a read is a lookup.
//!
//! What that costs, and how each cost is paid:
//!
//! - **The first read has nothing to look up.** It waits on a condition
//!   variable for at most [`COLD_STATUS`], then answers that it does not know.
//!   It never answers "off".
//! - **A cache can go stale without anyone noticing.** A snapshot is good for
//!   65 seconds while the stream is up and 5 seconds while it is not; past
//!   that a read is the failure that made it stale, or `Transport`.
//! - **A write is acknowledged before the bridge's own snapshots catch up.**
//!   The acknowledged state outranks an older observation for two seconds, so
//!   the read that follows a write agrees with it.
//!
//! Nothing here prints. The session runs inside the package executable, whose
//! stdout is the protocol socket.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    sync::{Arc, Condvar, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

use couch_sdk::LightState;
use serde_json::{json, Value};

use crate::{
    catalog::{Control, Kind},
    credential::HueCredential,
    tls, Deadlines, Error, Hue, Result,
};

/// How long a status read waits for the very first snapshot. Paid once per
/// child process, by whichever read arrives first.
pub const COLD_STATUS: Duration = Duration::from_secs(1);
/// A listing has nothing at all to show without a snapshot, and it is a
/// deliberate act - somebody opened the device picker - so it waits longer.
pub const COLD_LISTING: Duration = Duration::from_secs(4);
/// How long a snapshot is believed while the event stream is up. The stream
/// says when something changes, so age only proves the stream may have died
/// quietly.
const STREAMING_LIFE: Duration = Duration::from_secs(65);
/// ...and while it is not, when a poll is all that keeps it current.
const POLLING_LIFE: Duration = Duration::from_secs(5);
/// How long an acknowledged write outranks an older observation.
const SETTLING: Duration = Duration::from_secs(2);
/// A Hue bridge takes one command per grouped light per second; sent more, it
/// drops them. Couch refuses the second one instead, without a request.
pub const GROUP_INTERVAL: Duration = Duration::from_secs(1);
/// How often the poll re-reads the bridge while the stream is up.
const POLL_WITH_STREAM: Duration = Duration::from_secs(60);
const POLL_STEP: Duration = Duration::from_millis(500);

/// What a read found.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// A snapshot, and this child is in it.
    Known(Control),
    /// A snapshot, and this child is not in it: the bridge no longer has it.
    Missing,
    /// No snapshot yet, and nothing has gone wrong: the bridge has simply not
    /// been heard from. The answer is "unknown", never "off".
    Cold,
}

/// A write the bridge has acknowledged, held against older observations.
#[derive(Debug, Clone)]
struct Pending {
    state: LightState,
    until: Instant,
}

#[derive(Default)]
struct Cache {
    /// Every child, in the one order a listing ever uses.
    controls: Vec<Control>,
    index: HashMap<String, usize>,
    updated: Option<Instant>,
    streaming: bool,
    /// Why the last read of the bridge failed, if it did. A read finds this
    /// rather than a stale value.
    failure: Option<Error>,
    generation: u64,
    commanding: bool,
    dirty: bool,
    pending: HashMap<String, Pending>,
}

impl Cache {
    fn fresh(&self) -> bool {
        self.updated.is_some_and(|at| {
            at.elapsed()
                < if self.streaming {
                    STREAMING_LIFE
                } else {
                    POLLING_LIFE
                }
        })
    }

    fn invalidate(&mut self) {
        self.updated = None;
        self.generation += 1;
    }

    fn get(&self, id: &str) -> Option<&Control> {
        self.index.get(id).and_then(|at| self.controls.get(*at))
    }

    fn set(&mut self, control: Control) {
        match self.index.get(&control.id) {
            Some(at) => self.controls[*at] = control,
            None => {
                self.index.insert(control.id.clone(), self.controls.len());
                self.controls.push(control);
            }
        }
    }

    /// Replace everything with one read of the bridge, holding acknowledged
    /// writes over observations that have not caught up.
    fn apply(&mut self, mut controls: Vec<Control>, now: Instant) {
        self.pending.retain(|_, pending| now < pending.until);
        for control in &mut controls {
            if let Some(pending) = self.pending.get(&control.id) {
                // Unavailability and deletion stay authoritative: a lamp that
                // has left the mesh is unknown, whatever was asked of it.
                if control.state.on.is_some() {
                    control.state = pending.state;
                }
            }
        }
        self.index = controls
            .iter()
            .enumerate()
            .map(|(at, control)| (control.id.clone(), at))
            .collect();
        self.controls = controls;
        self.updated = Some(now);
    }
}

struct Stream {
    url: String,
    key: String,
    certificate: Vec<u8>,
}

/// One bridge, one key, one cache.
pub struct Session {
    reader: Hue,
    writer: Hue,
    stream: Stream,
    cache: Mutex<Cache>,
    /// Signalled when the first snapshot lands, or when reading the bridge
    /// fails. The only thing a request ever waits on.
    ready: Condvar,
    refreshing: Mutex<()>,
    /// When each grouped light was last written to.
    paced: Mutex<HashMap<String, Instant>>,
}

impl Session {
    /// A session for the key Couch is holding. **No network**: it returns as
    /// soon as the two clients are built, and the threads it starts do the
    /// talking. There is no file: the key and the certificate come from the
    /// credential and go nowhere else.
    pub fn from_credential(host: &str, credential: &HueCredential) -> Result<Arc<Self>> {
        let base = crate::base(host)?;
        let session = Arc::new(Self {
            reader: Hue::new_with(
                &base,
                &credential.application_key,
                &credential.certificate,
                Deadlines::Read,
            )?,
            writer: Hue::new_with(
                &base,
                &credential.application_key,
                &credential.certificate,
                Deadlines::Write,
            )?,
            stream: Stream {
                url: format!("{base}/eventstream/clip/v2"),
                key: credential.application_key.clone(),
                certificate: credential.certificate.clone(),
            },
            cache: Mutex::new(Cache::default()),
            ready: Condvar::new(),
            refreshing: Mutex::new(()),
            paced: Mutex::new(HashMap::new()),
        });
        for worker in [poll as fn(Weak<Session>), stream] {
            let weak = Arc::downgrade(&session);
            thread::Builder::new()
                .name("couch-hue".into())
                .spawn(move || worker(weak))
                .map_err(|_| Error::Transport)?;
        }
        Ok(session)
    }

    fn cache(&self) -> Result<std::sync::MutexGuard<'_, Cache>> {
        self.cache.lock().map_err(|_| Error::Response)
    }

    /// Wait, at most `patience`, for a snapshot or a failure. The only place
    /// a request blocks, and it blocks on the cache rather than on a socket.
    fn settled(&self, patience: Duration) -> Result<std::sync::MutexGuard<'_, Cache>> {
        let cache = self.cache()?;
        if cache.fresh() || cache.failure.is_some() {
            return Ok(cache);
        }
        self.ready
            .wait_timeout_while(cache, patience, |cache| {
                !cache.fresh() && cache.failure.is_none()
            })
            .map(|(cache, _)| cache)
            .map_err(|_| Error::Response)
    }

    /// One child, from the cache. Never a request.
    pub fn read(&self, id: &str) -> Result<Reading> {
        let cache = self.settled(COLD_STATUS)?;
        if !cache.fresh() {
            return match cache.failure.clone() {
                Some(failure) => Err(failure),
                None => Ok(Reading::Cold),
            };
        }
        Ok(match cache.get(id) {
            Some(control) => Reading::Known(control.clone()),
            None => Reading::Missing,
        })
    }

    /// The child a write is aimed at. A write has to know what the lamp can
    /// do, so unlike a read it cannot answer "unknown".
    pub fn control(&self, id: &str) -> Result<Control> {
        match self.read(id)? {
            Reading::Known(control) => Ok(control),
            Reading::Missing => Err(Error::Unavailable),
            Reading::Cold => Err(Error::Transport),
        }
    }

    /// Every child, in the one order this session ever lists them.
    pub fn children(&self) -> Result<Vec<Control>> {
        let cache = self.settled(COLD_LISTING)?;
        if cache.fresh() {
            return Ok(cache.controls.clone());
        }
        Err(cache.failure.clone().unwrap_or(Error::Transport))
    }

    /// Whether the event stream is up, which is what a snapshot's age means.
    pub fn streaming(&self) -> bool {
        self.cache().map(|cache| cache.streaming).unwrap_or(false)
    }

    /// A Hue bridge takes one command per grouped light per second. Refused
    /// here, before a request, because the bridge's own answer is to drop it
    /// silently and leave the room at a level nobody asked for.
    fn pace(&self, id: &str) -> Result<()> {
        let mut paced = self.paced.lock().map_err(|_| Error::Response)?;
        let now = Instant::now();
        if paced
            .get(id)
            .is_some_and(|last| now.duration_since(*last) < GROUP_INTERVAL)
        {
            return Err(Error::Paced);
        }
        paced.insert(id.to_string(), now);
        Ok(())
    }

    /// Send one write and hold its result against older observations.
    ///
    /// `target` is what the child will be in afterwards, which is what the
    /// caller is acknowledged with: the read that follows a write has to
    /// agree with the write.
    pub fn write(&self, control: &Control, body: Value, target: LightState) -> Result<LightState> {
        let (kind, id) = control.endpoint();
        if control.kind == Kind::Group {
            self.pace(&control.id)?;
        }
        {
            let mut cache = self.cache()?;
            cache.generation += 1;
            cache.commanding = true;
        }
        let result = self.writer.write_resource(kind, id, body);
        let mut cache = self.cache()?;
        cache.commanding = false;
        cache.generation += 1;
        cache.dirty = true;
        if let Err(error) = result {
            cache.pending.remove(&control.id);
            if error == Error::Authentication {
                // The key stopped working. A later read has to say so rather
                // than serve what the bridge said while it still did.
                cache.invalidate();
                cache.failure = Some(error.clone());
            }
            return Err(error);
        }
        cache.pending.insert(
            control.id.clone(),
            Pending {
                state: target,
                until: Instant::now() + SETTLING,
            },
        );
        let mut settled = control.clone();
        settled.state = target;
        cache.set(settled);
        Ok(target)
    }

    /// Recall a scene. A scene has no state, so there is nothing to hold: the
    /// cache is simply due a re-read, which the poll does within a step.
    pub fn recall(&self, control: &Control) -> Result<()> {
        if control.kind != Kind::Scene {
            return Err(Error::Configuration);
        }
        let (kind, id) = control.endpoint();
        self.writer
            .write_resource(kind, id, json!({"recall": {"action": "active"}}))?;
        let mut cache = self.cache()?;
        // A scene moves lamps this session did not write to, so nothing that
        // was acknowledged before it is worth holding any more.
        cache.pending.clear();
        cache.dirty = true;
        Ok(())
    }

    /// Read the bridge and replace the snapshot. Only ever called from a
    /// background thread.
    fn refresh(&self) -> Result<()> {
        let Ok(_busy) = self.refreshing.try_lock() else {
            // Somebody else is already reading it; ask them to do it again
            // rather than opening a second connection.
            self.cache()?.dirty = true;
            return Ok(());
        };
        let generation = {
            let mut cache = self.cache()?;
            cache.dirty = false;
            cache.generation
        };
        let result = self.reader.catalog();
        let mut cache = self.cache()?;
        // A read that started before a write must not undo it.
        if cache.generation != generation || cache.commanding {
            cache.dirty = true;
            return Ok(());
        }
        let answer = match result {
            Ok(controls) => {
                cache.apply(controls, Instant::now());
                cache.failure = None;
                Ok(())
            }
            Err(error) => {
                cache.invalidate();
                cache.failure = Some(error.clone());
                Err(error)
            }
        };
        drop(cache);
        self.ready.notify_all();
        answer
    }
}

/// Re-read the bridge: on a schedule, and whenever something asked for it.
fn poll(weak: Weak<Session>) {
    let mut last: Option<Instant> = None;
    loop {
        let Some(session) = weak.upgrade() else {
            return;
        };
        let (streaming, dirty) = match session.cache() {
            Ok(cache) => (cache.streaming, cache.dirty),
            Err(_) => return,
        };
        let interval = if streaming {
            POLL_WITH_STREAM
        } else {
            POLLING_LIFE
        };
        if dirty || last.is_none_or(|at| at.elapsed() >= interval) {
            let _ = session.refresh();
            last = Some(Instant::now());
        }
        drop(session);
        thread::sleep(POLL_STEP);
    }
}

/// Hold the bridge's event stream open. It does not carry state this package
/// trusts; it carries "something changed", and the answer to that is a read.
fn stream(weak: Weak<Session>) {
    let Some(session) = weak.upgrade() else {
        return;
    };
    let agent = tls::stream_agent(Arc::new(Mutex::new(session.stream.certificate.clone())));
    let url = session.stream.url.clone();
    let key = session.stream.key.clone();
    drop(session);
    let mut backoff = 1;
    while weak.strong_count() > 0 {
        let result = (|| -> Result<()> {
            let response = agent
                .get(&url)
                .header("hue-application-key", &key)
                .header("Accept", "text/event-stream")
                .call()
                .map_err(crate::transport)?;
            if !response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                return Err(Error::Response);
            }
            {
                let Some(session) = weak.upgrade() else {
                    return Ok(());
                };
                session.cache()?.streaming = true;
                // Read after subscribing, so a change made while the stream
                // was down is not missed.
                session.refresh()?;
            }
            let mut reader = BufReader::new(response.into_body().into_reader());
            loop {
                let changed = event(&mut reader)?;
                backoff = 1;
                let Some(session) = weak.upgrade() else {
                    return Ok(());
                };
                if changed {
                    let _ = session.refresh();
                }
            }
        })();
        let Some(session) = weak.upgrade() else {
            return;
        };
        if let Ok(mut cache) = session.cache() {
            cache.streaming = false;
            cache.invalidate();
        }
        // Without the stream the snapshot is only worth five seconds, so find
        // out at once whether the bridge is there at all.
        let _ = session.refresh();
        drop(session);
        if result.is_ok() {
            return;
        }
        thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(30);
    }
}

/// Parse one server-sent event, including comments, CRLF and multi-line data.
/// Both lines and events are bounded, so a bridge that never ends one cannot
/// grow this process without limit.
fn event(reader: &mut impl BufRead) -> Result<bool> {
    use std::io::Read;
    let mut data = String::new();
    loop {
        let mut line = String::new();
        let read = reader
            .take(65537)
            .read_line(&mut line)
            .map_err(|_| Error::Transport)?;
        if read == 0 {
            return Err(Error::Transport);
        }
        if read > 65536 {
            return Err(Error::Response);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if data.is_empty() {
                return Ok(false);
            }
            let value: Value = serde_json::from_str(&data).map_err(|_| Error::Response)?;
            let events = value.as_array().ok_or(Error::Response)?;
            return Ok(events
                .iter()
                .any(|event| matches!(event["type"].as_str(), Some("update" | "add" | "delete"))));
        }
        if let Some(part) = line.strip_prefix("data:") {
            data.push_str(part.strip_prefix(' ').unwrap_or(part));
            data.push('\n');
            if data.len() > 131_072 {
                return Err(Error::Response);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    const LAMP: &str = "00000000-0000-4000-8000-000000000001";

    fn lamp(on: Option<bool>, brightness: Option<u8>) -> Control {
        Control {
            id: LAMP.into(),
            kind: Kind::Light,
            name: "Reading lamp".into(),
            room_hint: Some("Living room".into()),
            traits: couch_sdk::LightTraits {
                dimmable: true,
                mirek: Some((153, 500)),
                color: false,
            },
            state: LightState {
                on,
                brightness,
                mirek: None,
                xy: None,
            },
        }
    }

    /// A session against a plain-HTTP fixture, with no threads: what the
    /// threads do is exercised by the package's own tests, against the fake
    /// bridge over TLS.
    fn session(address: std::net::SocketAddr) -> Session {
        let client = || Hue {
            base: format!("http://{address}"),
            key: "fixture".into(),
            agent: ureq::Agent::new_with_defaults(),
        };
        Session {
            reader: client(),
            writer: client(),
            stream: Stream {
                url: String::new(),
                key: String::new(),
                certificate: Vec::new(),
            },
            cache: Mutex::new(Cache::default()),
            ready: Condvar::new(),
            refreshing: Mutex::new(()),
            paced: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn an_acknowledged_write_outranks_an_older_observation_for_two_seconds() {
        let mut cache = Cache::default();
        cache.apply(vec![lamp(Some(false), Some(56))], Instant::now());
        assert!(cache.fresh());
        let acknowledged = LightState {
            on: Some(true),
            brightness: Some(56),
            mirek: None,
            xy: None,
        };
        cache.pending.insert(
            LAMP.into(),
            Pending {
                state: acknowledged,
                until: Instant::now() + SETTLING,
            },
        );

        // The bridge keeps saying the old thing while it settles.
        for observed in [Some(false), Some(true), Some(false)] {
            cache.apply(vec![lamp(observed, Some(20))], Instant::now());
            assert_eq!(cache.get(LAMP).unwrap().state, acknowledged);
        }
        // Unavailability is not an older observation: it is news.
        cache.apply(vec![lamp(None, None)], Instant::now());
        assert_eq!(cache.get(LAMP).unwrap().state.on, None);
        // And once it has settled, the bridge is right again.
        let expired = cache.pending[LAMP].until + Duration::from_millis(1);
        cache.apply(vec![lamp(Some(false), Some(20))], expired);
        assert_eq!(cache.get(LAMP).unwrap().state.brightness, Some(20));
        assert!(cache.pending.is_empty());
        // A child the bridge no longer has is gone, not remembered.
        cache.apply(Vec::new(), Instant::now());
        assert_eq!(cache.get(LAMP), None);
    }

    #[test]
    fn a_snapshot_is_worth_five_seconds_without_the_stream_and_sixty_five_with_it() {
        let mut cache = Cache::default();
        assert!(!cache.fresh(), "nothing has been read yet");
        cache.updated = Some(Instant::now() - Duration::from_secs(10));
        assert!(!cache.fresh());
        cache.streaming = true;
        assert!(cache.fresh());
        cache.updated = Some(Instant::now() - Duration::from_secs(66));
        assert!(!cache.fresh());
        cache.updated = Some(Instant::now());
        cache.invalidate();
        assert!(!cache.fresh());
    }

    #[test]
    fn a_cold_read_waits_once_and_then_says_it_does_not_know() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let session = session(listener.local_addr().unwrap());
        let started = Instant::now();
        assert_eq!(session.read(LAMP).unwrap(), Reading::Cold);
        assert!(started.elapsed() >= COLD_STATUS, "it has to wait");
        assert!(
            started.elapsed() < COLD_STATUS * 3,
            "and then it has to answer"
        );
        // A write cannot answer "unknown": it needs to know what the lamp is.
        assert_eq!(session.control(LAMP).unwrap_err(), Error::Transport);
        drop(listener);
    }

    #[test]
    fn a_failure_is_what_a_read_finds_rather_than_a_stale_value() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let session = session(listener.local_addr().unwrap());
        session
            .cache()
            .unwrap()
            .apply(vec![lamp(Some(true), Some(50))], Instant::now());
        assert!(matches!(session.read(LAMP).unwrap(), Reading::Known(_)));
        {
            let mut cache = session.cache().unwrap();
            cache.invalidate();
            cache.failure = Some(Error::Authentication);
        }
        let started = Instant::now();
        assert_eq!(session.read(LAMP).unwrap_err(), Error::Authentication);
        assert!(
            started.elapsed() < COLD_STATUS,
            "a known failure is not waited out"
        );
        assert_eq!(session.children().unwrap_err(), Error::Authentication);
        drop(listener);
    }

    #[test]
    fn a_room_takes_one_command_a_second_and_the_second_one_is_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let session = session(listener.local_addr().unwrap());
        assert_eq!(session.pace("room/a"), Ok(()));
        assert_eq!(session.pace("room/a"), Err(Error::Paced));
        // A different room is a different bridge command.
        assert_eq!(session.pace("room/b"), Ok(()));
        drop(listener);
    }

    #[test]
    fn an_event_is_parsed_with_its_comments_its_line_endings_and_its_limits() {
        assert!(!event(&mut &b": heartbeat\r\n\r\n"[..]).unwrap());
        assert!(event(&mut &b"id: 1\ndata: [\ndata: {\"type\":\"delete\"}]\n\n"[..]).unwrap());
        assert!(!event(&mut &b"data: [{\"type\":\"hello\"}]\n\n"[..]).unwrap());
        assert!(event(&mut &b"data: broken\n\n"[..]).is_err());
        assert!(event(&mut &vec![b'x'; 65537][..]).is_err());
        assert!(event(&mut &b""[..]).is_err());
    }

    #[test]
    fn a_read_that_started_before_a_write_cannot_undo_it() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        let (finish, release) = std::sync::mpsc::channel();
        let remote = thread::spawn(move || {
            let read = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(read.method(), &tiny_http::Method::Get);
            ready.send(()).unwrap();
            let write = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(write.method(), &tiny_http::Method::Put);
            write
                .respond(tiny_http::Response::from_string(
                    json!({"errors": [], "data": [{"rid": LAMP, "rtype": "light"}]}).to_string(),
                ))
                .unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
            // The read finishes last, and says what the bridge said before.
            read.respond(tiny_http::Response::from_string(
                json!({"errors": [], "data": [
                    {"id": LAMP, "type": "light", "owner": {"rid": "d0"},
                     "metadata": {"name": "Reading lamp"}, "on": {"on": false},
                     "dimming": {"brightness": 10.0}},
                    {"id": "z0", "type": "zigbee_connectivity", "owner": {"rid": "d0"},
                     "status": "connected"},
                ]})
                .to_string(),
            ))
            .unwrap();
        });

        let session = Arc::new(session(address));
        session
            .cache()
            .unwrap()
            .apply(vec![lamp(Some(false), Some(10))], Instant::now());
        let background = session.clone();
        let reading = thread::spawn(move || background.refresh());
        wait.recv_timeout(Duration::from_secs(5)).unwrap();

        let control = session.control(LAMP).unwrap();
        let target = LightState {
            on: Some(true),
            ..control.state
        };
        assert_eq!(
            session
                .write(&control, json!({"on": {"on": true}}), target)
                .unwrap(),
            target
        );
        finish.send(()).unwrap();
        reading.join().unwrap().unwrap();
        remote.join().unwrap();
        assert_eq!(session.control(LAMP).unwrap().state.on, Some(true));
    }

    #[test]
    fn what_a_write_is_acknowledged_with_is_what_the_next_read_returns() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let remote = thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                request.url(),
                format!("/clip/v2/resource/light/{LAMP}").as_str()
            );
            request
                .respond(tiny_http::Response::from_string(
                    json!({"errors": [], "data": [{"rid": LAMP, "rtype": "light"}]}).to_string(),
                ))
                .unwrap();
        });
        let session = session(address);
        session
            .cache()
            .unwrap()
            .apply(vec![lamp(Some(false), Some(10))], Instant::now());
        let control = session.control(LAMP).unwrap();
        let target = LightState {
            on: Some(true),
            brightness: Some(70),
            mirek: None,
            xy: None,
        };
        let acknowledged = session
            .write(
                &control,
                json!({"on": {"on": true}, "dimming": {"brightness": 70}}),
                target,
            )
            .unwrap();
        assert_eq!(acknowledged, target);
        assert_eq!(session.control(LAMP).unwrap().state, target);
        remote.join().unwrap();
    }

    #[test]
    fn the_cache_holds_exactly_what_the_catalogue_read_produced() {
        let controls = catalog::catalog(&[json!({"id": LAMP, "type": "light",
            "metadata": {"name": "Reading lamp"}, "on": {"on": true}})]);
        let mut cache = Cache::default();
        cache.apply(controls.clone(), Instant::now());
        assert_eq!(cache.controls, controls);
        assert_eq!(cache.get(LAMP), controls.first());
    }
}
