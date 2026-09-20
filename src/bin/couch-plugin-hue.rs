//! Standalone integration adapter. stdout is reserved for framed protocol:
//! one stray byte written by this process retires the connection, so nothing
//! under `src/` prints.
fn main() {
    let manifest = serde_json::from_str(include_str!("../../plugin.json"))
        .expect("embedded integration manifest");
    if couch_plugin::serve::<couch_hue::adapter::HueBridge>(manifest).is_err() {
        std::process::exit(1);
    }
}
