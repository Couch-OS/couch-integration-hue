//! A package slot: the real executable at the path its manifest names.
//!
//! `couch_plugin::testing::Package` does this too, but it starts an endpoint
//! with no credential, and a package whose manifest says `pairing.required`
//! answers `unpaired` to everything without one. Until the harness can hand a
//! key over (core gap G1, fixed at T7) a package that pairs has to build its
//! own slot to be tested paired at all.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use couch_plugin::{Endpoint, HostPolicy, Manifest};
use couch_sdk::Credential;
use serde_json::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Slot {
    root: PathBuf,
    pub manifest: Manifest,
}

impl Slot {
    /// The package as it is installed: an immutable directory with the
    /// executable at `manifest.executable`.
    pub fn new() -> Self {
        let manifest: Manifest = serde_json::from_str(include_str!("../../plugin.json"))
            .expect("the embedded integration manifest");
        manifest.validate().expect("a valid integration manifest");
        let binary = Path::new(env!("CARGO_BIN_EXE_couch-plugin-hue"));
        let parent = binary.parent().expect("the test binary's directory");
        let root = loop {
            let candidate = parent.join(format!(
                ".couch-hue-slot-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create the package slot: {error}"),
            }
        };
        let executable = root.join(&manifest.executable);
        std::fs::create_dir_all(executable.parent().expect("bin/")).expect("the slot's bin/");
        // Linux can inherit another test thread's write-open copy and refuse
        // the exec with ETXTBSY; the Cargo artifact is already immutable and
        // on the same filesystem, so link it there rather than copying.
        #[cfg(target_os = "linux")]
        std::fs::hard_link(binary, &executable).expect("link the package binary");
        #[cfg(not(target_os = "linux"))]
        std::fs::copy(binary, &executable).expect("copy the package binary");
        Self { root, manifest }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The endpoint the daemon would start, with the key Couch holds for this
    /// connection - or without one, which is a connection that is not paired.
    pub fn endpoint(
        &self,
        settings: Value,
        credential: Option<&Credential>,
        timeout: Duration,
    ) -> Endpoint {
        Endpoint::start_paired(
            &self.root,
            self.manifest.clone(),
            settings,
            credential,
            timeout,
            HostPolicy::default(),
        )
        .expect("a configured package endpoint")
    }
}

impl Default for Slot {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
