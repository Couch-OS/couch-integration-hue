//! Shared protocol 3 admission checks against the real package and fake TLS bridge.

mod fake;

use couch_plugin::{
    testing::{self, Adapter},
    testing_v3::{self, ChildrenCase},
};
use couch_sdk::TypedAction;
use fake::bridge::{self, FakeBridge};

fn adapter() -> Adapter<'static> {
    Adapter {
        binary: std::path::Path::new(env!("CARGO_BIN_EXE_couch-plugin-hue")),
        manifest_json: include_str!("../plugin.json"),
    }
}

#[test]
fn children() {
    let bridge = FakeBridge::start();
    let credential = bridge.credential();
    testing_v3::children(
        adapter(),
        ChildrenCase {
            device: Box::new(bridge),
            credential: Some(credential),
            expect: bridge::CHILDREN,
            kind: "light",
            write: TypedAction::SetLight {
                on: Some(true),
                brightness: Some(40),
                mirek: None,
                xy: None,
            },
            unknown: "no-such-lamp",
        },
    );
}

#[test]
fn concurrent_package_startup_is_offline_and_race_free() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let package = testing::Package::new(adapter());
                let mut host = package.host();
                assert_eq!(
                    host.configure(serde_json::json!({"host": "192.0.2.1"})),
                    Ok(())
                );
            })
        })
        .collect();
    barrier.wait();
    for thread in threads {
        thread.join().expect("concurrent package startup");
    }
}
