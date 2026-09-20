//! One read of the bridge, turned into the children of a Couch connection.
//!
//! A bridge answers `GET /clip/v2/resource` with everything it has, flat: a
//! lamp, the device the lamp is a service of, that device's zigbee link, the
//! room the device is a child of, the grouped light that room is controlled
//! by, the zones, and every scene with the group it belongs to. This module
//! is the one place that graph is walked, and what comes out is the three
//! kinds of child the manifest declares and nothing else.
//!
//! Three things are decided here and nowhere else:
//!
//! - **The id.** A lamp is its own v2 UUID, a room is `room/<grouped_light
//!   uuid>` and a scene is `scene/<uuid>`. They are what a person's saved
//!   configuration holds, so they may never be reassigned.
//! - **The room hint.** A lamp's room is the room whose `children` list its
//!   *device*; a room's hint is its own name; a scene's is the name of the
//!   room or zone it belongs to. It is a hint for the person choosing devices
//!   and nothing more: Couch never puts a device in a room by itself.
//! - **What a lamp can do.** Dimmable is a `dimming` service; a colour
//!   temperature range is a `mirek_schema`; colour is not supported in this
//!   version and the trait is always false, whatever the lamp says.

use std::collections::BTreeMap;

use couch_sdk::{LightState, LightTraits, MAX_CHILD_LABEL};
use serde_json::Value;

use crate::{valid_id, Hue, Result};

/// The three kinds of child, in the order a listing sorts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Group,
    Light,
    Scene,
}

impl Kind {
    /// Exactly the kind names `plugin.json` declares.
    pub fn name(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Light => "light",
            Self::Scene => "scene",
        }
    }
    pub fn from_resource(id: &str) -> Self {
        if id.starts_with("scene/") {
            Self::Scene
        } else if id.starts_with("room/") {
            Self::Group
        } else {
            Self::Light
        }
    }
}

/// One child of the connection, as the bridge last described it.
#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    /// The resource id Couch saves and sends back: never reassigned.
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub room_hint: Option<String>,
    /// What this particular lamp or room can do. All false for a scene.
    pub traits: LightTraits,
    /// What it was doing. Every field is `None` when the bridge did not say,
    /// which for a lamp off the mesh means unknown and never "off".
    pub state: LightState,
}

impl Control {
    /// The bridge resource a write goes to: its type and its UUID.
    pub fn endpoint(&self) -> (&'static str, &str) {
        match self.kind {
            Kind::Light => ("light", self.id.as_str()),
            Kind::Group => ("grouped_light", &self.id["room/".len()..]),
            Kind::Scene => ("scene", &self.id["scene/".len()..]),
        }
    }
}

/// Text fit to put in front of a person: no control characters, never empty,
/// and inside the limit the host checks every child against.
fn shown(text: Option<&str>, fallback: &str) -> String {
    let mut text: String = text
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if text.trim().is_empty() {
        text = fallback.to_string();
    }
    if text.len() > MAX_CHILD_LABEL {
        let mut end = MAX_CHILD_LABEL;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn percent(value: &Value) -> Option<u8> {
    value
        .as_f64()
        .filter(|level| level.is_finite() && (0.0..=100.0).contains(level))
        .map(|level| level.round() as u8)
}

fn mirek_range(resource: &Value) -> Option<(u16, u16)> {
    let schema = &resource["color_temperature"]["mirek_schema"];
    let low = u16::try_from(schema["mirek_minimum"].as_u64()?).ok()?;
    let high = u16::try_from(schema["mirek_maximum"].as_u64()?).ok()?;
    let range = (low, high);
    // A lamp that reports a range Couch cannot hold is a lamp with no colour
    // temperature, rather than a child the host would refuse outright.
    (low <= high
        && LightTraits {
            dimmable: false,
            mirek: Some(range),
            color: false,
        }
        .is_valid())
    .then_some(range)
}

fn mirek_now(resource: &Value, range: Option<(u16, u16)>) -> Option<u16> {
    let (low, high) = range?;
    let value = u16::try_from(resource["color_temperature"]["mirek"].as_u64()?).ok()?;
    (low..=high).contains(&value).then_some(value)
}

impl Hue {
    /// One read of the bridge as the package's children, sorted the one way it
    /// ever lists them.
    pub fn catalog(&self) -> Result<Vec<Control>> {
        let all = self.raw_resources()?;
        Ok(catalog(&all))
    }
}

/// Walk one `GET /clip/v2/resource` answer.
///
/// Nothing here is an error: a bridge that leaves something out has fewer
/// children, not a broken connection. A resource whose id is not a v2 UUID is
/// skipped, because that id would go into a person's saved configuration.
pub fn catalog(all: &[Value]) -> Vec<Control> {
    let named = |group: &Value| shown(group["metadata"]["name"].as_str(), "Hue room");

    // Which room each device (and, for a zone, each light) belongs to, and
    // what every room and zone is called.
    let mut room_of: BTreeMap<&str, String> = BTreeMap::new();
    let mut group_name: BTreeMap<&str, String> = BTreeMap::new();
    for group in all
        .iter()
        .filter(|value| value["type"] == "room" || value["type"] == "zone")
    {
        let Some(id) = group["id"].as_str() else {
            continue;
        };
        let name = named(group);
        group_name.insert(id, name.clone());
        // Only a room puts a lamp in a room. A zone is a second grouping of
        // the same lamps, and letting it win would move a lamp out of the
        // room a person keeps it in.
        if group["type"] != "room" {
            continue;
        }
        for child in group["children"].as_array().into_iter().flatten() {
            if let Some(rid) = child["rid"].as_str() {
                room_of.insert(rid, name.clone());
            }
        }
    }

    let mut controls = Vec::new();
    for lamp in all.iter().filter(|value| value["type"] == "light") {
        let Some(id) = lamp["id"].as_str().filter(|id| valid_id(id)) else {
            continue;
        };
        let owner = lamp["owner"]["rid"].as_str();
        let connected = owner.is_some_and(|owner| {
            all.iter().any(|value| {
                value["type"] == "zigbee_connectivity"
                    && value["owner"]["rid"].as_str() == Some(owner)
                    && value["status"] == "connected"
            })
        });
        let range = mirek_range(lamp);
        let on = connected.then(|| lamp["on"]["on"].as_bool()).flatten();
        controls.push(Control {
            id: id.to_string(),
            kind: Kind::Light,
            name: shown(lamp["metadata"]["name"].as_str(), "Hue light"),
            // A lamp is in the room its *device* is a child of. Hue puts the
            // device in the room, not the light service.
            room_hint: owner
                .and_then(|owner| room_of.get(owner))
                .or_else(|| room_of.get(id))
                .cloned(),
            traits: LightTraits {
                dimmable: lamp["dimming"].is_object(),
                mirek: range,
                color: false,
            },
            state: LightState {
                on,
                // Hue keeps the level it will return to while a lamp is off,
                // so this is reported whenever the lamp answered at all.
                brightness: on.and_then(|_| percent(&lamp["dimming"]["brightness"])),
                mirek: on.and_then(|_| mirek_now(lamp, range)),
                xy: None,
            },
        });
    }

    // Rooms, through the grouped light that controls them. Zones are not
    // children in this version: a zone is a second grouping of lamps that are
    // already in a room, and two sliders for one lamp is worse than one.
    for room in all.iter().filter(|value| value["type"] == "room") {
        let name = named(room);
        let group = room["services"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|service| service["rtype"] == "grouped_light")
            .and_then(|service| service["rid"].as_str())
            .filter(|id| valid_id(id));
        let Some(group) = group else {
            continue;
        };
        let state = all
            .iter()
            .find(|value| value["type"] == "grouped_light" && value["id"] == group);
        let range = state.and_then(mirek_range);
        let on = state.and_then(|state| state["on"]["on"].as_bool());
        controls.push(Control {
            id: format!("room/{group}"),
            kind: Kind::Group,
            name: name.clone(),
            room_hint: Some(name),
            traits: LightTraits {
                dimmable: state.is_some_and(|state| state["dimming"].is_object()),
                mirek: range,
                color: false,
            },
            state: LightState {
                on,
                brightness: on
                    .and_then(|_| state)
                    .and_then(|state| percent(&state["dimming"]["brightness"])),
                mirek: on
                    .and_then(|_| state)
                    .and_then(|state| mirek_now(state, range)),
                xy: None,
            },
        });
    }

    for scene in all.iter().filter(|value| value["type"] == "scene") {
        let Some(id) = scene["id"].as_str().filter(|id| valid_id(id)) else {
            continue;
        };
        controls.push(Control {
            id: format!("scene/{id}"),
            kind: Kind::Scene,
            name: shown(scene["metadata"]["name"].as_str(), "Hue scene"),
            // A scene belongs to a room or a zone, and a zone's scenes are
            // listed under the zone's name even though the zone is not a
            // child: that is where the person will look for them.
            room_hint: scene["group"]["rid"]
                .as_str()
                .and_then(|group| group_name.get(group))
                .cloned(),
            traits: LightTraits::default(),
            state: LightState::default(),
        });
    }

    // One order, always: an id that shifted between two pages of one listing
    // would cost the package its child process.
    controls.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    controls
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lamp(index: u8) -> String {
        format!("00000000-0000-4000-8000-{index:012x}")
    }

    fn household() -> Vec<Value> {
        vec![
            json!({"id": "d0", "type": "device", "metadata": {"name": "Lamp"},
                   "services": [{"rid": lamp(1), "rtype": "light"}]}),
            json!({"id": "z0", "type": "zigbee_connectivity", "owner": {"rid": "d0"},
                   "status": "connected"}),
            json!({"id": lamp(1), "type": "light", "owner": {"rid": "d0"},
                   "metadata": {"name": "Reading lamp"}, "on": {"on": true},
                   "dimming": {"brightness": 42.4},
                   "color_temperature": {"mirek": 300, "mirek_valid": true,
                       "mirek_schema": {"mirek_minimum": 153, "mirek_maximum": 500}},
                   "color": {"xy": {"x": 0.4, "y": 0.4}}}),
            json!({"id": "d1", "type": "device", "metadata": {"name": "Off the mesh"},
                   "services": [{"rid": lamp(2), "rtype": "light"}]}),
            json!({"id": "z1", "type": "zigbee_connectivity", "owner": {"rid": "d1"},
                   "status": "connectivity_issue"}),
            json!({"id": lamp(2), "type": "light", "owner": {"rid": "d1"},
                   "metadata": {"name": "Alcove"}, "on": {"on": true},
                   "dimming": {"brightness": 80.0}}),
            json!({"id": "r0", "type": "room", "metadata": {"name": "Living room"},
                   "children": [{"rid": "d0", "rtype": "device"}, {"rid": "d1", "rtype": "device"}],
                   "services": [{"rid": lamp(3), "rtype": "grouped_light"}]}),
            json!({"id": lamp(3), "type": "grouped_light", "owner": {"rid": "r0", "rtype": "room"},
                   "on": {"on": false}, "dimming": {"brightness": 60.0}}),
            json!({"id": "n0", "type": "zone", "metadata": {"name": "Downstairs"},
                   "children": [{"rid": lamp(1), "rtype": "light"}],
                   "services": [{"rid": lamp(4), "rtype": "grouped_light"}]}),
            json!({"id": lamp(4), "type": "grouped_light", "owner": {"rid": "n0", "rtype": "zone"},
                   "on": {"on": true}}),
            json!({"id": lamp(5), "type": "scene", "metadata": {"name": "Evening"},
                   "group": {"rid": "r0", "rtype": "room"}}),
            json!({"id": lamp(6), "type": "scene", "metadata": {"name": "Away"},
                   "group": {"rid": "n0", "rtype": "zone"}}),
        ]
    }

    #[test]
    fn a_bridge_becomes_three_kinds_of_child_and_a_zone_is_not_one_of_them() {
        let controls = catalog(&household());
        let ids: Vec<&str> = controls.iter().map(|c| c.id.as_str()).collect();
        // Sorted by kind, then name, then id: group, then lights, then scenes.
        assert_eq!(
            ids,
            [
                format!("room/{}", lamp(3)).as_str(),
                lamp(2).as_str(),
                lamp(1).as_str(),
                format!("scene/{}", lamp(6)).as_str(),
                format!("scene/{}", lamp(5)).as_str(),
            ]
        );
        // The zone's grouped light is not a child; the zone's scene is.
        assert!(!ids.iter().any(|id| id.contains(&lamp(4))));

        let group = &controls[0];
        assert_eq!(group.kind, Kind::Group);
        assert_eq!(group.name, "Living room");
        assert_eq!(group.room_hint.as_deref(), Some("Living room"));
        assert_eq!(group.state.on, Some(false));
        assert_eq!(group.state.brightness, Some(60));
        assert!(group.traits.dimmable);
        assert_eq!(group.endpoint(), ("grouped_light", lamp(3).as_str()));

        // The lamp that is not on the mesh: unknown, never an inferred off.
        let alcove = &controls[1];
        assert_eq!(alcove.name, "Alcove");
        assert_eq!(alcove.state.on, None);
        assert_eq!(alcove.state.brightness, None);
        assert_eq!(alcove.room_hint.as_deref(), Some("Living room"));

        let reading = &controls[2];
        assert_eq!(reading.state.on, Some(true));
        assert_eq!(reading.state.brightness, Some(42));
        assert_eq!(reading.state.mirek, Some(300));
        assert_eq!(reading.traits.mirek, Some((153, 500)));
        // The lamp says it has colour; this version says it has not.
        assert!(!reading.traits.color);
        assert_eq!(reading.state.xy, None);
        assert_eq!(reading.endpoint(), ("light", lamp(1).as_str()));

        // A zone's scene is listed, under the zone's name.
        let away = &controls[3];
        assert_eq!(away.kind, Kind::Scene);
        assert_eq!(away.name, "Away");
        assert_eq!(away.room_hint.as_deref(), Some("Downstairs"));
        assert_eq!(away.endpoint(), ("scene", lamp(6).as_str()));
        assert_eq!(controls[4].room_hint.as_deref(), Some("Living room"));
        assert!(controls
            .iter()
            .all(|control| control.traits.is_valid() && control.state.is_valid()));
    }

    #[test]
    fn what_a_bridge_leaves_out_costs_a_child_and_never_the_connection() {
        // No id, a nonsense id, no room, no grouped light, no metadata.
        let controls = catalog(&[
            json!({"type": "light", "metadata": {"name": "No id"}}),
            json!({"id": "../escape", "type": "light", "metadata": {"name": "Climbing"}}),
            json!({"id": lamp(1), "type": "light", "on": {"on": true}}),
            json!({"id": "r0", "type": "room", "metadata": {"name": "Empty"}, "services": []}),
            json!({"id": lamp(2), "type": "scene", "group": {"rid": "gone"}}),
        ]);
        assert_eq!(controls.len(), 2);
        // No zigbee link at all, so its state is unknown.
        assert_eq!(controls[0].name, "Hue light");
        assert_eq!(controls[0].state.on, None);
        assert_eq!(controls[0].room_hint, None);
        assert_eq!(controls[1].name, "Hue scene");
        assert_eq!(controls[1].room_hint, None);
    }

    #[test]
    fn a_name_is_always_something_a_host_will_accept() {
        assert_eq!(shown(None, "Hue light"), "Hue light");
        assert_eq!(shown(Some("   "), "Hue light"), "Hue light");
        assert_eq!(shown(Some("Two\nlines"), "x"), "Twolines");
        let long = "é".repeat(200);
        let cut = shown(Some(&long), "x");
        assert!(cut.len() <= MAX_CHILD_LABEL && !cut.is_empty());
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_colour_temperature_range_couch_cannot_hold_is_no_range_at_all() {
        let outside = json!({"color_temperature": {"mirek_schema":
            {"mirek_minimum": 1, "mirek_maximum": 99999}}});
        assert_eq!(mirek_range(&outside), None);
        let backwards = json!({"color_temperature": {"mirek_schema":
            {"mirek_minimum": 500, "mirek_maximum": 153}}});
        assert_eq!(mirek_range(&backwards), None);
        let ordinary = json!({"color_temperature": {"mirek": 200,
            "mirek_schema": {"mirek_minimum": 153, "mirek_maximum": 500}}});
        assert_eq!(mirek_range(&ordinary), Some((153, 500)));
        assert_eq!(mirek_now(&ordinary, Some((153, 500))), Some(200));
        assert_eq!(mirek_now(&ordinary, Some((250, 500))), None);
    }
}
