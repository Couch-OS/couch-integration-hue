//! The observed state of one Hue control and the commands it accepts.
//!
//! The Couch monorepo shares these two types with its Home Assistant client
//! (`couch_ha::{Light, Command}`). They are defined here so this crate stands
//! alone; the serialized shape of [`Light`] is the one Couch already stores
//! and renders, and `tests/shape.rs` holds it still.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Light {
    /// A light's v2 UUID, `room:<grouped_light UUID>` or `scene:<scene UUID>`.
    pub entity_id: String,
    pub name: String,
    /// None means unknown/unavailable, never an inferred off state.
    pub on: Option<bool>,
    pub brightness_percent: Option<u8>,
    pub dimmable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    On,
    Off,
    Brightness(u8),
}
