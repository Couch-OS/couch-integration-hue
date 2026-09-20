//! What the package is on the outside: its two manifests, its silence on
//! stdout, and what it answers before Couch holds a key for it.

mod fake;

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::Duration,
};

use couch_plugin::{Error, Manifest, Request};
use fake::{bridge::FakeBridge, slot};
use serde_json::{json, Value};

const PLUGIN: &str = include_str!("../plugin.json");
const INTEGRATION: &str = include_str!("../integration.json");
const CARGO: &str = include_str!("../Cargo.toml");

/// The version has two spellings, on purpose: Cargo needs semver's hyphen and
/// `abuild` refuses one, so the package, the APK and the feed all say
/// `0.1.0_pre1` and only `Cargo.toml` says `0.1.0-pre1`.
const APK_VERSION: &str = "0.1.0_pre2";
const CARGO_VERSION: &str = "0.1.0-pre2";

#[test]
fn the_two_manifests_say_the_same_package_and_the_feed_could_read_them() {
    let manifest: Manifest = serde_json::from_str(PLUGIN).expect("plugin.json parses");
    manifest
        .validate()
        .expect("plugin.json is a valid manifest");
    let integration: Value = serde_json::from_str(INTEGRATION).expect("integration.json parses");
    let mut keys: Vec<&str> = integration
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    // The feed reads exactly these nine, and refuses a tenth.
    assert_eq!(
        keys,
        [
            "binary",
            "cargo_manifest",
            "cargo_package",
            "id",
            "manifest",
            "protocol_version",
            "schema",
            "synthetic",
            "tier",
        ]
    );
    assert_eq!(integration["protocol_version"], 3);
    assert_eq!(manifest.protocol_version, 3);
    // `min_core_protocol_version` lives in plugin.json and must equal it.
    assert_eq!(manifest.min_core_protocol_version, 3);
    assert_eq!(integration["id"], "hue");
    assert_eq!(manifest.id, "hue");
    assert_eq!(integration["tier"], "preview");
    assert_eq!(integration["synthetic"], false);
    assert_eq!(integration["cargo_package"], "couch-hue");
    assert_eq!(integration["binary"], "couch-plugin-hue");
    assert_eq!(integration["manifest"], "plugin.json");
    assert_eq!(manifest.executable, "bin/couch-plugin-hue");

    // `build-apk.sh` refuses to build unless the manifest's version is the APK
    // version exactly, so the two spellings have to stay where they are.
    assert_eq!(manifest.version, APK_VERSION);
    assert!(
        CARGO.contains(&format!("version = \"{CARGO_VERSION}\"")),
        "Cargo.toml must carry the semver spelling of {APK_VERSION}"
    );
    assert_eq!(APK_VERSION.replace('_', "-"), CARGO_VERSION);

    // Pairing is required, and the child is kept alive: the bridge's event
    // stream is worth nothing if the process holding it is reaped when a
    // panel goes idle.
    let pairing = manifest.pairing.expect("the package pairs");
    assert!(pairing.required);
    assert_eq!(pairing.max_seconds, 120);
    assert!(manifest.keep_alive);
    assert!(
        manifest.capabilities.is_empty(),
        "the connection is a bridge"
    );
    assert_eq!(manifest.settings.len(), 1);
    assert_eq!(manifest.settings[0].id, "host");
    assert!(manifest.settings[0].required);

    let kinds: Vec<&str> = manifest
        .children
        .iter()
        .map(|kind| kind.kind.as_str())
        .collect();
    assert_eq!(kinds, ["light", "group", "scene"]);
}

#[test]
fn nothing_in_the_library_writes_to_stdout() {
    // The package executable's stdout is the protocol socket. One line of
    // chatter in a library file corrupts a frame and retires the child, and
    // the fault would be somewhere quite else. `src/main.rs` is the command
    // line, which the package never runs.
    let mut offenders = Vec::new();
    let mut files = Vec::new();
    for directory in ["src", "src/bin"] {
        for entry in std::fs::read_dir(directory).expect("the source tree") {
            let path = entry.expect("a source file").path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    assert!(files.len() >= 8, "the source tree moved: {files:?}");
    for path in files {
        if path.file_name().is_some_and(|name| name == "main.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a source file");
        for (number, line) in text.lines().enumerate() {
            if line.contains("println!")
                || line.contains("print!")
                || line.contains("dbg!")
                || line.contains("io::stdout")
                || line.contains("stdout()")
            {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "stdout is the protocol socket:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_package_puts_nothing_on_stdout_but_protocol_frames() {
    // The real executable, driven over real pipes, through a whole session:
    // handshake, configure with a key, a listing, a read and a write. Every
    // byte that comes back has to be a frame, and when the conversation ends
    // there must be nothing left over - not a warning, not a blank line.
    let bridge = FakeBridge::start();
    let mut child = Command::new(env!("CARGO_BIN_EXE_couch-plugin-hue"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the package binary");
    let mut input = child.stdin.take().expect("its stdin");
    let mut output = child.stdout.take().expect("its stdout");

    let settings = json!({"host": bridge.address().to_string()});
    let credential = bridge.credential();
    let mut answers = Vec::new();
    let mut exchange = |body: Value, answers: &mut Vec<Value>| {
        write_frame(&mut input, &json!({"id": answers.len() + 1, "body": body}));
        answers.push(read_frame(&mut output));
    };
    exchange(
        json!({"method": "hello", "protocol_version": 3}),
        &mut answers,
    );
    exchange(
        serde_json::to_value(Request::configure_with(settings.clone(), Some(&credential))).unwrap(),
        &mut answers,
    );
    // A pairing conversation, which is where a package is most likely to say
    // something out loud: nobody has pressed this bridge's button, so it
    // stays open long enough to be polled and then closed.
    exchange(
        serde_json::to_value(Request::pair_start(settings, None)).unwrap(),
        &mut answers,
    );
    let session = answers[2]["body"]["session"]
        .as_str()
        .expect("a pairing session")
        .to_string();
    exchange(
        serde_json::to_value(Request::pair_continue(&session, None)).unwrap(),
        &mut answers,
    );
    exchange(
        serde_json::to_value(Request::pair_cancel(&session)).unwrap(),
        &mut answers,
    );
    exchange(
        serde_json::to_value(Request::children(None)).unwrap(),
        &mut answers,
    );
    exchange(
        serde_json::to_value(Request::status().at(bridge.light_child(0))).unwrap(),
        &mut answers,
    );
    exchange(
        serde_json::to_value(Request::command("on").at(bridge.light_child(0))).unwrap(),
        &mut answers,
    );
    drop(exchange);
    drop(input);

    let mut leftover = Vec::new();
    output
        .read_to_end(&mut leftover)
        .expect("the end of stdout");
    assert!(
        leftover.is_empty(),
        "stdout carried {} bytes that were not a frame: {:?}",
        leftover.len(),
        String::from_utf8_lossy(&leftover),
    );
    let _ = child.wait();

    for (index, answer) in answers.iter().enumerate() {
        assert_eq!(
            answer["id"],
            index as u64 + 1,
            "a reply out of order: {answer}"
        );
        assert!(answer["body"]["type"].is_string(), "{answer}");
        assert!(
            answer.get("store_credential").is_none(),
            "this package never rotates a key"
        );
    }
    assert_eq!(answers[0]["body"]["type"], "hello");
    assert_eq!(answers[0]["body"]["manifest"]["id"], "hue");
    assert_eq!(answers[1]["body"]["type"], "ok");
    assert_eq!(answers[2]["body"]["type"], "pairing");
    assert_eq!(answers[2]["body"]["step"]["step"], "waiting");
    assert_eq!(answers[3]["body"]["type"], "pairing");
    assert_eq!(answers[4]["body"]["type"], "ok", "a cancel is answered");
    // Whatever the answers to the last three are, nothing anywhere in them may
    // repeat the key or the certificate.
    let transcript = serde_json::to_string(&answers).expect("the transcript");
    assert!(!transcript.contains(bridge.application_key()));
    assert!(!transcript.contains(&couch_hue::credential::encode(&bridge.certificate())));
}

#[test]
fn a_connection_couch_holds_no_key_for_is_unpaired_and_opens_no_socket() {
    let bridge = FakeBridge::start();
    let slot = slot::shared();
    let endpoint = slot.endpoint(bridge.settings_value(), None, Duration::from_secs(5));
    for request in [
        Request::children(None),
        Request::status().at(bridge.light_child(0)),
        Request::command("on").at(bridge.light_child(0)),
    ] {
        let kind = request.resource().map(|_| "light");
        assert_eq!(
            endpoint
                .request_child_detailed(kind, request.clone())
                .map_err(|failure| failure.code),
            Err(Error::Unpaired),
            "{request:?} must be refused without a key",
        );
    }
    assert!(
        bridge.requests().is_empty(),
        "an unpaired client must not address the bridge: {:?}",
        bridge.requests()
    );
}

// ---------------------------------------------------------------------------
// The framing, written out by hand so that this test does not borrow the
// reader it is checking.
// ---------------------------------------------------------------------------

fn write_frame(writer: &mut impl Write, frame: &Value) {
    let bytes = serde_json::to_vec(frame).expect("a frame");
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .expect("the length");
    writer.write_all(&bytes).expect("the frame");
    writer.flush().expect("the frame");
}

fn read_frame(reader: &mut impl Read) -> Value {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).expect("a frame length");
    let size = u32::from_be_bytes(length) as usize;
    assert!(size > 0 && size <= 64 * 1024, "a frame of {size} bytes");
    let mut bytes = vec![0u8; size];
    reader.read_exact(&mut bytes).expect("a frame body");
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "stdout carried something that is not a frame ({error}): {:?}",
            String::from_utf8_lossy(&bytes)
        )
    })
}
