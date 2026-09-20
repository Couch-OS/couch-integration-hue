//! The package: one Couch connection, and a Hue bridge's lights, rooms and
//! scenes behind it as children.
//!
//! Three kinds of child, exactly as `plugin.json` declares them: a `light` is
//! one lamp, a `group` is one *room*'s grouped light, and a `scene` is a scene
//! that is recalled by being told `on`. The ids are the bridge's own v2 UUIDs:
//! `<light uuid>`, `room/<grouped_light uuid>` and `scene/<scene uuid>`.
//!
//! What this file decides:
//!
//! - **Reads come out of the cache** ([`crate::session`]), so a panel with
//!   twelve lamps on it costs the bridge nothing.
//! - **A listing is frozen** when it starts. The event stream can reorder the
//!   bridge between two pages of one listing, and a child that appeared twice
//!   would cost this package its process.
//! - **A write is refused here if the lamp cannot do it**: a brightness to a
//!   lamp with no dimming service, a colour temperature to one with no range,
//!   a colour to anything. None of those costs a round trip.
//! - **Every refusal carries a sentence for the person**, and none of them
//!   names a key, a certificate or a path.

use std::sync::{Arc, LazyLock};

use couch_sdk::{
    couch_model::{
        commands::{Function, KeyPhase},
        ChildComponent, DeviceKind, PluginCapability,
    },
    Capability, Child, ChildPage, ClientSettings, Credential, DeviceClient, Error as SdkError,
    LightState, PluginActionSchema, PluginChildKind, Reason, Result as SdkResult, Status,
    TypedAction,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    catalog::{Control, Kind},
    credential::HueCredential,
    pairing::HueFlow,
    session::{Reading, Session},
    Error,
};

/// What the owner types: the bridge's address on the LAN. Everything else a
/// bridge needs is in the credential Couch keeps for the connection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HueSettings {
    pub host: String,
}

impl HueSettings {
    /// The error the settings form can mark, with a sentence for the person.
    pub fn invalid() -> SdkError {
        SdkError::Invalid.because(Reason::InvalidSetting {
            field: "host".into(),
            text: "Enter the Hue bridge's address on your network".into(),
        })
    }
}

impl ClientSettings for HueSettings {
    const FILE_PREFIX: &'static str = "hue";

    fn validate(&self) -> SdkResult<()> {
        crate::base(&self.host)
            .map(|_| ())
            .map_err(|_| Self::invalid())
    }
}

fn capability(id: &str, label: &str) -> PluginCapability {
    PluginCapability {
        id: id.into(),
        label: label.into(),
    }
}

/// The kinds of child, exactly as `plugin.json` declares them. `serve` refuses
/// to start if the two ever disagree.
pub static KINDS: LazyLock<Vec<PluginChildKind>> = LazyLock::new(|| {
    let switchable = || {
        vec![
            capability("on", "On"),
            capability("off", "Off"),
            capability("toggle", "Toggle"),
        ]
    };
    vec![
        PluginChildKind {
            kind: "light".into(),
            label: "Hue light".into(),
            device_kind: DeviceKind::Light,
            component: ChildComponent::Light,
            capabilities: switchable(),
            actions: vec![PluginActionSchema::SetLight {}],
        },
        PluginChildKind {
            kind: "group".into(),
            label: "Hue room".into(),
            device_kind: DeviceKind::Light,
            component: ChildComponent::Light,
            capabilities: switchable(),
            actions: vec![PluginActionSchema::SetLight {}],
        },
        PluginChildKind {
            kind: "scene".into(),
            label: "Hue scene".into(),
            device_kind: DeviceKind::Other,
            component: ChildComponent::Scene,
            capabilities: vec![capability("on", "Recall")],
            actions: vec![],
        },
    ]
});

/// The sentences this package shows a person. Each one says what happened and
/// what to do; none of them says anything about a key or a certificate.
const UNREACHABLE: &str = "This Hue light is not answering the bridge";
const VANISHED: &str = "This is no longer on the Hue bridge";
const NOT_DIMMABLE: &str = "This Hue light cannot be dimmed";
const NO_TEMPERATURE: &str = "This Hue light has no colour temperature";
const NO_COLOUR: &str = "Couch cannot set a Hue light's colour yet";
const REFUSED: &str = "The Hue bridge refused that";

fn message(text: &str) -> Reason {
    Reason::Message { text: text.into() }
}

/// Every error this package reports, in the words Couch shows and the code it
/// acts on. A key, a certificate and a path appear in none of them.
fn refused(error: Error) -> SdkError {
    match error {
        Error::Configuration => HueSettings::invalid(),
        // The bridge no longer accepts the key Couch holds; only pairing
        // again fixes it, and `Unpaired` is what tells Couch to offer that.
        Error::Authentication | Error::LinkButton => {
            SdkError::Unpaired.because(message(crate::REPAIR))
        }
        Error::Transport => SdkError::Transport,
        Error::Response => SdkError::Protocol,
        Error::Unavailable => SdkError::Rejected.because(message(VANISHED)),
        Error::Brightness => SdkError::Invalid.because(message(NOT_DIMMABLE)),
        Error::Rejected => SdkError::Rejected.because(message(REFUSED)),
    }
}

/// One child, as a listing describes it.
fn described(control: &Control) -> Child {
    let mut child = Child::new(&control.id, control.kind.name(), &control.name);
    if let Some(hint) = &control.room_hint {
        child = child.in_room(hint);
    }
    // A scene is drawn with the scene control, which has no traits at all;
    // the host refuses a child whose traits are not its kind's.
    if control.kind != Kind::Scene {
        child = child.with_light(control.traits);
    }
    child
}

/// One Couch connection to one bridge.
pub struct HueBridge {
    settings: HueSettings,
    /// `None` until Couch holds a key for this connection. Such a client opens
    /// no socket and answers [`SdkError::Unpaired`] to everything, which is
    /// what lets the owner save the address before pairing.
    session: Option<Arc<Session>>,
    /// One listing's children, frozen when it started. The bridge's event
    /// stream can reorder the cache between two pages, and a child that came
    /// back twice would cost this package its process.
    listing: Option<Vec<Child>>,
}

impl HueBridge {
    fn session(&self) -> SdkResult<&Arc<Session>> {
        self.session.as_ref().ok_or(SdkError::Unpaired)
    }

    /// The child a write is aimed at, or the reason there is not one.
    fn control(&self, resource: &str) -> SdkResult<Control> {
        self.session()?.control(resource).map_err(refused)
    }

    /// What a write leaves behind, which is what it is acknowledged with.
    fn acknowledged(
        &self,
        control: &Control,
        body: Value,
        target: LightState,
    ) -> SdkResult<Status> {
        let state = self
            .session()?
            .write(control, body, target)
            .map_err(refused)?;
        Ok(Status::default().with_light(state))
    }

    /// Turn one child on or off, from what the cache says it is now.
    fn switch(&self, control: &Control, on: bool) -> SdkResult<Status> {
        let target = LightState {
            on: Some(on),
            ..control.state
        };
        self.acknowledged(control, json!({"on": {"on": on}}), target)
    }
}

impl DeviceClient for HueBridge {
    type Settings = HueSettings;

    const KIND: &'static str = "hue";
    const LABEL: &'static str = "Philips Hue";

    /// The connection itself is not a device: every lamp, room and scene is a
    /// child of it.
    fn capabilities() -> &'static [Capability] {
        &[]
    }

    fn child_kinds() -> &'static [PluginChildKind] {
        &KINDS
    }

    fn connect(settings: &HueSettings) -> SdkResult<Self> {
        Self::connect_with(settings, None)
    }

    /// No network, and it returns at once - with a key or without one. A
    /// bridge is spoken to by the session's own threads, never on the way in.
    fn connect_with(settings: &HueSettings, credential: Option<&Credential>) -> SdkResult<Self> {
        settings.validate()?;
        let session = match credential {
            None => None,
            Some(credential) => {
                let credential = HueCredential::parse(credential)
                    .map_err(|_| SdkError::Unpaired.because(message(crate::REPAIR)))?;
                Some(Session::from_credential(&settings.host, &credential).map_err(refused)?)
            }
        };
        Ok(Self {
            settings: settings.clone(),
            session,
            listing: None,
        })
    }

    /// Begin a pairing conversation with the bridge these settings address.
    ///
    /// It needs no prior configure and no key: a Hue connection becomes
    /// usable by being paired. `existing` is ignored, because a Hue bridge
    /// issues a new application key every time the button is pressed and has
    /// no notion of one key standing for another - so re-pairing starts
    /// clean, and the old key stays valid on the bridge until its owner
    /// deletes it there.
    fn pair_start(
        settings: &HueSettings,
        _existing: Option<&Credential>,
    ) -> SdkResult<Box<dyn couch_sdk::PairFlow>> {
        settings.validate()?;
        Ok(Box::new(HueFlow::new(settings).map_err(refused)?))
    }

    /// The connection has no commands of its own.
    fn execute(&mut self, _function: &Function) -> SdkResult<()> {
        Err(SdkError::Unsupported)
    }

    /// One page of the bridge's lamps, rooms and scenes.
    ///
    /// The whole listing is taken once, when the first page is asked for, and
    /// later pages are cut from that same snapshot: the cursor is an id, and
    /// an id that moved between two pages would repeat or skip a child.
    fn children(&mut self, cursor: Option<&str>) -> SdkResult<ChildPage> {
        let session = self.session()?.clone();
        if cursor.is_none() || self.listing.is_none() {
            let controls = session.children().map_err(refused)?;
            self.listing = Some(controls.iter().map(described).collect());
        }
        let frozen = self.listing.clone().unwrap_or_default();
        ChildPage::fill(frozen, cursor)
    }

    /// What one child is doing, from the cache. Never a request.
    fn child_status(&mut self, resource: &str) -> SdkResult<Status> {
        match self.session()?.read(resource).map_err(refused)? {
            Reading::Known(control) if control.kind == Kind::Scene => Ok(Status::default()),
            Reading::Known(control) => Ok(Status::default().with_light(control.state)),
            // The bridge has been read and does not have this. Answering for
            // something else would be worse than refusing.
            Reading::Missing => Err(SdkError::Rejected.because(message(VANISHED))),
            // Nothing has been read yet. Unknown, and never an inferred off.
            Reading::Cold if Kind::from_resource(resource) == Kind::Scene => Ok(Status::default()),
            Reading::Cold => Ok(Status::default().with_light(LightState::default())),
        }
    }

    fn child_command(
        &mut self,
        resource: &str,
        function: &Function,
        _phase: KeyPhase,
    ) -> SdkResult<Option<Status>> {
        let control = self.control(resource)?;
        match (control.kind, function) {
            // A scene is not a switch: `on` recalls it and there is nothing
            // to report afterwards.
            (Kind::Scene, Function::On) => {
                self.session()?.recall(&control).map_err(refused)?;
                Ok(None)
            }
            (Kind::Light | Kind::Group, Function::On) => self.switch(&control, true).map(Some),
            (Kind::Light | Kind::Group, Function::Off) => self.switch(&control, false).map(Some),
            // Toggling is a decision, and it is made from what the cache
            // says. A lamp whose state is unknown cannot be toggled into a
            // guess.
            (Kind::Light | Kind::Group, Function::Toggle) => {
                let on = control
                    .state
                    .on
                    .ok_or_else(|| SdkError::Rejected.because(message(UNREACHABLE)))?;
                self.switch(&control, !on).map(Some)
            }
            _ => Err(SdkError::Unsupported),
        }
    }

    /// A brightness, a colour temperature, or both, with the power.
    ///
    /// Every refusal here happens before a request: what one lamp accepts is
    /// a trait of that lamp, and the bridge's own answer to an impossible
    /// write is a 200 with an error inside it that says nothing useful.
    fn child_action(&mut self, resource: &str, action: TypedAction) -> SdkResult<Option<Status>> {
        let TypedAction::SetLight {
            on,
            brightness,
            mirek,
            xy,
        } = action
        else {
            return Err(SdkError::Unsupported);
        };
        if xy.is_some() {
            return Err(SdkError::Invalid.because(message(NO_COLOUR)));
        }
        let control = self.control(resource)?;
        if control.kind == Kind::Scene {
            return Err(SdkError::Unsupported);
        }
        // Brightness 0 is off, as it is in built-in Hue, and a lamp that
        // cannot be dimmed can still be switched off.
        if brightness.is_some_and(|level| level > 0) && !control.traits.dimmable {
            return Err(SdkError::Invalid.because(message(NOT_DIMMABLE)));
        }
        let Some((coolest, warmest)) = control.traits.mirek else {
            if mirek.is_some() {
                return Err(SdkError::Invalid.because(message(NO_TEMPERATURE)));
            }
            return self.write_light(&control, on, brightness, None);
        };
        let mirek = mirek.map(|value| value.clamp(coolest, warmest));
        self.write_light(&control, on, brightness, mirek)
    }
}

impl HueBridge {
    /// Build the one request, and what the child will be afterwards.
    fn write_light(
        &self,
        control: &Control,
        on: Option<bool>,
        brightness: Option<u8>,
        mirek: Option<u16>,
    ) -> SdkResult<Option<Status>> {
        let mut body = Map::new();
        let mut target = control.state;
        if let Some(on) = on {
            body.insert("on".into(), json!({"on": on}));
            target.on = Some(on);
        }
        match brightness {
            // Zero is off, and Hue keeps the level the lamp will come back to.
            Some(0) => {
                body.insert("on".into(), json!({"on": false}));
                target.on = Some(false);
            }
            Some(level) => {
                body.insert("dimming".into(), json!({"brightness": level}));
                target.brightness = Some(level);
                // Dimming a lamp that is off turns it on, unless the same
                // request also said to switch it off.
                if on != Some(false) {
                    body.insert("on".into(), json!({"on": true}));
                    target.on = Some(true);
                }
            }
            None => (),
        }
        if let Some(mirek) = mirek {
            body.insert("color_temperature".into(), json!({"mirek": mirek}));
            target.mirek = Some(mirek);
        }
        if body.is_empty() {
            return Err(SdkError::Invalid.because(message("That asks the Hue bridge for nothing")));
        }
        self.acknowledged(control, Value::Object(body), target)
            .map(Some)
    }

    /// The address this connection was configured with, for the pairing flow
    /// that will need it.
    pub fn host(&self) -> &str {
        &self.settings.host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_kinds_are_the_manifest_s_kinds() {
        let manifest: couch_plugin::Manifest =
            serde_json::from_str(include_str!("../plugin.json")).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.children, *KINDS);
        assert_eq!(manifest.id, HueBridge::KIND);
        assert!(manifest.capabilities.is_empty());
        assert!(manifest.pairs());
        assert!(manifest.keep_alive);
    }

    #[test]
    fn a_client_with_no_key_is_unpaired_and_never_addresses_a_bridge() {
        let settings = HueSettings {
            host: "192.0.2.10".into(),
        };
        let mut client = HueBridge::connect_with(&settings, None).unwrap();
        assert_eq!(client.children(None).unwrap_err(), SdkError::Unpaired);
        assert_eq!(client.child_status("abc").unwrap_err(), SdkError::Unpaired);
        assert_eq!(
            client
                .child_action(
                    "abc",
                    TypedAction::SetLight {
                        on: Some(true),
                        brightness: None,
                        mirek: None,
                        xy: None
                    }
                )
                .unwrap_err(),
            SdkError::Unpaired
        );
        assert_eq!(client.host(), "192.0.2.10");
        assert!(HueBridge::connect_with(&HueSettings::default(), None).is_err());
    }

    #[test]
    fn every_refusal_carries_a_sentence_and_none_of_them_names_a_secret() {
        for error in [
            Error::Configuration,
            Error::LinkButton,
            Error::Authentication,
            Error::Transport,
            Error::Response,
            Error::Unavailable,
            Error::Brightness,
            Error::Rejected,
        ] {
            let refusal = refused(error.clone());
            let text = refusal.message();
            assert!(!text.is_empty(), "{error:?}");
            assert!(text.len() <= Reason::MAX_TEXT, "{error:?}: {text}");
            assert!(!text.chars().any(char::is_control), "{error:?}");
            // A sentence may say that the key stopped working; it may never
            // carry one, nor a certificate, nor a path. (That no real key
            // ever reaches a reply is asserted end to end in tests/plugin.rs
            // and tests/children.rs, against the fake bridge's own key.)
            for forbidden in ["hue-application-key", "/opt/", "BEGIN ", "0x"] {
                assert!(!text.contains(forbidden), "{error:?}: {text}");
            }
            assert!(!text.contains('/'), "{error:?}: {text}");
        }
        // Only these two ask the person to pair again.
        assert_eq!(refused(Error::Authentication).code(), &SdkError::Unpaired);
        assert_eq!(refused(Error::Transport).code(), &SdkError::Transport);
        assert_eq!(
            refused(Error::Configuration).reason().unwrap().field(),
            Some("host")
        );
    }

    #[test]
    fn a_scene_is_described_without_a_lamp_s_traits() {
        let scene = Control {
            id: "scene/00000000-0000-4000-8000-000000000001".into(),
            kind: Kind::Scene,
            name: "Evening".into(),
            room_hint: Some("Living room".into()),
            traits: Default::default(),
            state: LightState::default(),
        };
        let child = described(&scene);
        assert_eq!(child.kind, "scene");
        assert_eq!(child.light, None);
        assert_eq!(child.room_hint.as_deref(), Some("Living room"));
        assert!(child.is_well_formed());
        assert!(child.snapshot().fits(ChildComponent::Scene));

        let lamp = Control {
            kind: Kind::Light,
            id: "00000000-0000-4000-8000-000000000002".into(),
            name: "Reading lamp".into(),
            ..scene
        };
        let child = described(&lamp);
        assert_eq!(child.light, Some(lamp.traits));
        assert!(child.is_well_formed());
        assert!(child.snapshot().fits(ChildComponent::Light));
    }
}
