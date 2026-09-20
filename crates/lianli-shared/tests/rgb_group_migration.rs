use lianli_shared::rgb::{RgbDeviceConfig, RgbRegionConfig};

fn legacy() -> RgbDeviceConfig {
    serde_json::from_value(serde_json::json!({
        "device_id": "hid:old:group0",
        "zones": [{"zone_index": 0, "effect": {
            "mode": "Breathing", "colors": [[10,20,30]], "scope": "Inner",
            "disabled": true, "brightness": 255, "speed": 1
        }, "swap_lr": true, "swap_tb": true}]
    }))
    .unwrap()
}

#[test]
fn expansion_preserves_effects_and_is_idempotent_across_serialization() {
    let mut config = legacy();
    let original = config.zones[0].clone();
    assert!(config.expand_legacy_group_zone(6));
    for (index, zone) in config.zones.iter().enumerate() {
        assert_eq!(zone.zone_index as usize, index);
        assert_eq!(zone.effect, original.effect);
        assert!(zone.swap_lr && zone.swap_tb);
    }
    let value = serde_json::to_value(&config).unwrap();
    let mut reloaded: RgbDeviceConfig = serde_json::from_value(value.clone()).unwrap();
    assert!(!reloaded.expand_legacy_group_zone(6));
    assert_eq!(serde_json::to_value(reloaded).unwrap(), value);
}

#[test]
fn explicit_partial_fan_and_ring_settings_are_preserved() {
    let mut partial = legacy();
    let mut off = partial.zones[0].clone();
    off.zone_index = 2;
    partial.zones.push(off);
    let mut rings = legacy();
    rings.regions = Some(vec![RgbRegionConfig {
        effect: rings.zones[0].effect.clone(),
        flip: false,
    }]);
    let mut other_zone = legacy();
    other_zone.zones[0].zone_index = 1;
    let mut empty = legacy();
    empty.zones.clear();
    for mut config in [partial, rings, other_zone, empty] {
        let original = config.clone();
        assert!(!config.expand_legacy_group_zone(6));
        assert_eq!(config, original);
    }
}
