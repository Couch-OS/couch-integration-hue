//! Couch stores and renders this exact shape; the monorepo shares it with its
//! Home Assistant client. A renamed or dropped field is a host-visible change.
use couch_hue::Light;
use serde_json::json;

#[test]
fn light_keeps_the_shape_couch_already_reads() {
    let light = Light {
        entity_id: "room:00000000-0000-0000-0000-000000000001".into(),
        name: "Office".into(),
        on: Some(true),
        brightness_percent: Some(42),
        dimmable: true,
    };
    let value = serde_json::to_value(&light).unwrap();
    assert_eq!(
        value,
        json!({"entity_id":"room:00000000-0000-0000-0000-000000000001","name":"Office",
            "on":true,"brightness_percent":42,"dimmable":true})
    );
    assert_eq!(serde_json::from_value::<Light>(value).unwrap(), light);
    let unknown: Light = serde_json::from_value(json!({"entity_id":"x","name":"x",
        "on":null,"brightness_percent":null,"dimmable":false}))
    .unwrap();
    assert_eq!(unknown.on, None);
}
