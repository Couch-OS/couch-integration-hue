//! The package: one Couch connection, and a Hue bridge's lights, rooms and
//! scenes behind it as children.
//!
//! Three kinds of child, exactly as `plugin.json` declares them: a `light` is
//! one lamp, a `group` is one *room*'s grouped light, and a `scene` is a scene
//! that is recalled by being told `on`. The ids are the bridge's own v2 UUIDs:
//! `<light uuid>`, `room/<grouped_light uuid>` and `scene/<scene uuid>`.
//!
//! This is the skeleton: it declares what the package is, holds the settings
//! and the key, and refuses everything it cannot yet do. The session that
//! keeps a bridge's state current, and the writes that go to it, arrive next.

use std::sync::LazyLock;

use couch_sdk::{
    couch_model::{
        commands::{Function, KeyPhase},
        ChildComponent, DeviceKind, PluginCapability,
    },
    Capability, ChildPage, ClientSettings, Credential, DeviceClient, Error as SdkError,
    PluginActionSchema, PluginChildKind, Reason, Result as SdkResult, Status,
};
use serde::{Deserialize, Serialize};

use crate::credential::HueCredential;

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

/// One Couch connection to one bridge.
pub struct HueBridge {
    #[allow(dead_code)]
    settings: HueSettings,
    /// `None` until Couch holds a key for this connection. Such a client opens
    /// no socket and answers [`SdkError::Unpaired`] to everything, which is
    /// what lets the owner save the address before pairing.
    credential: Option<HueCredential>,
}

impl HueBridge {
    fn paired(&self) -> SdkResult<&HueCredential> {
        self.credential.as_ref().ok_or(SdkError::Unpaired)
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

    /// No network, and it returns at once. A bridge is spoken to when a
    /// request needs it, never on the way in.
    fn connect_with(settings: &HueSettings, credential: Option<&Credential>) -> SdkResult<Self> {
        settings.validate()?;
        let credential = match credential {
            None => None,
            Some(credential) => Some(HueCredential::parse(credential).map_err(|_| {
                SdkError::Unpaired.because(Reason::Message {
                    text: crate::REPAIR.into(),
                })
            })?),
        };
        Ok(Self {
            settings: settings.clone(),
            credential,
        })
    }

    fn execute(&mut self, _function: &Function) -> SdkResult<()> {
        Err(SdkError::Unsupported)
    }

    fn children(&mut self, _cursor: Option<&str>) -> SdkResult<ChildPage> {
        self.paired()?;
        Err(SdkError::Unsupported)
    }

    fn child_status(&mut self, _resource: &str) -> SdkResult<Status> {
        self.paired()?;
        Err(SdkError::Unsupported)
    }

    fn child_command(
        &mut self,
        _resource: &str,
        _function: &Function,
        _phase: KeyPhase,
    ) -> SdkResult<Option<Status>> {
        self.paired()?;
        Err(SdkError::Unsupported)
    }

    fn child_action(
        &mut self,
        _resource: &str,
        _action: couch_sdk::TypedAction,
    ) -> SdkResult<Option<Status>> {
        self.paired()?;
        Err(SdkError::Unsupported)
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
        assert!(HueBridge::connect_with(&HueSettings::default(), None).is_err());
    }
}
