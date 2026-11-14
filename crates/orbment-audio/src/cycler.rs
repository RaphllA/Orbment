use std::collections::HashSet;

use anyhow::{Result, bail};
use orbment_core::{
    config::{AudioDeviceSelection, CycleMode},
    i18n::tr,
};

pub(crate) struct Endpoints {
    pub connected: Vec<AudioDeviceSelection>,
    pub offline_ids: HashSet<String>,
}

impl Endpoints {
    /// Keep known IDs (even offline). Recover a missing ID by name only when the
    /// two lists give a one-to-one match and the online ID has no selection of its own.
    pub fn reconcile(&self, selected: &mut Vec<AudioDeviceSelection>) -> bool {
        let mut seen = HashSet::new();
        let mut next = Vec::with_capacity(selected.len());
        for saved in selected.iter() {
            let device = self
                .connected
                .iter()
                .find(|device| device.id == saved.id)
                .or_else(|| {
                    if self.offline_ids.contains(&saved.id)
                        || saved.name.is_empty()
                        || selected
                            .iter()
                            .any(|other| other.id != saved.id && other.name == saved.name)
                    {
                        return None;
                    }
                    let mut matches = self
                        .connected
                        .iter()
                        .filter(|device| device.name == saved.name);
                    let device = matches.next()?;
                    (matches.next().is_none()
                        && !selected.iter().any(|other| other.id == device.id))
                    .then_some(device)
                })
                .unwrap_or(saved);
            if seen.insert(device.id.as_str()) {
                next.push(device.clone());
            }
        }
        if *selected == next {
            return false;
        }
        *selected = next;
        true
    }
}

pub(crate) fn next_device<'a>(
    connected: &'a [AudioDeviceSelection],
    selected: &[AudioDeviceSelection],
    mode: &CycleMode,
    current: Option<&str>,
) -> Result<&'a AudioDeviceSelection> {
    let candidates: Vec<&AudioDeviceSelection> = match mode {
        CycleMode::All => connected.iter().collect(),
        CycleMode::Available => {
            let mut devices: Vec<&AudioDeviceSelection> = Vec::new();
            for selection in selected {
                if let Some(device) = connected.iter().find(|device| device.id == selection.id)
                    && !devices.iter().any(|candidate| candidate.id == device.id)
                {
                    devices.push(device);
                }
            }
            if devices.len() < 2 && connected.len() >= 2 {
                return next_device(connected, selected, &CycleMode::All, current);
            }
            devices
        }
    };
    if candidates.is_empty() {
        bail!(tr("audio.no_available_devices"));
    }
    let next = current
        .and_then(|id| candidates.iter().position(|device| device.id == id))
        .map_or(0, |index| (index + 1) % candidates.len());
    Ok(candidates[next])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> AudioDeviceSelection {
        AudioDeviceSelection {
            id: id.into(),
            name: format!("Device {id}"),
        }
    }

    #[test]
    fn available_keeps_selection_order_skips_disconnected_and_wraps() {
        let connected = vec![device("a"), device("b"), device("c")];
        let selected = vec![device("c"), device("gone"), device("a")];
        assert_eq!(
            next_device(&connected, &selected, &CycleMode::Available, Some("c"))
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            next_device(&connected, &selected, &CycleMode::Available, Some("a"))
                .unwrap()
                .id,
            "c"
        );
        assert_eq!(
            next_device(&connected, &selected, &CycleMode::Available, Some("b"))
                .unwrap()
                .id,
            "c"
        );
    }

    #[test]
    fn all_ignores_selections_and_uses_every_connected_device() {
        let connected = vec![device("a"), device("b"), device("c")];
        assert_eq!(
            next_device(&connected, &[device("a")], &CycleMode::All, Some("a"))
                .unwrap()
                .id,
            "b"
        );
        assert_eq!(
            next_device(&connected, &[], &CycleMode::All, Some("c"))
                .unwrap()
                .id,
            "a"
        );
    }

    #[test]
    fn empty_selection_never_switches_to_an_arbitrary_device() {
        assert!(next_device(&[device("a")], &[], &CycleMode::Available, None).is_err());
        assert!(
            next_device(
                &[device("a")],
                &[device("gone")],
                &CycleMode::Available,
                None
            )
            .is_err()
        );
        assert!(next_device(&[], &[], &CycleMode::All, None).is_err());
    }

    #[test]
    fn missing_default_selects_first_and_single_device_is_stable() {
        let connected = vec![device("a")];
        assert_eq!(
            next_device(&connected, &[], &CycleMode::All, None)
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            next_device(&connected, &[], &CycleMode::All, Some("a"))
                .unwrap()
                .id,
            "a"
        );
    }

    #[test]
    fn duplicate_selection_does_not_stall_cycle_and_uses_current_name() {
        let connected = vec![device("a"), device("b")];
        let selected = vec![device("a"), device("a"), device("b")];
        assert_eq!(
            next_device(&connected, &selected, &CycleMode::Available, Some("a"))
                .unwrap()
                .id,
            "b"
        );
        let stale = vec![AudioDeviceSelection {
            id: "a".into(),
            name: "Old name".into(),
        }];
        assert_eq!(
            next_device(&connected, &stale, &CycleMode::Available, None)
                .unwrap()
                .name,
            "Device a"
        );
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    fn device(id: &str, name: &str) -> AudioDeviceSelection {
        AudioDeviceSelection {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn available_falls_back_when_fewer_than_two_selections_match() {
        let connected = [device("a", "A"), device("b", "B")];
        for selected in [
            vec![],
            vec![device("gone", "Gone")],
            vec![device("a", "A")],
            vec![device("a", "A"), device("a", "A")],
        ] {
            assert_eq!(
                next_device(&connected, &selected, &CycleMode::Available, Some("a"))
                    .unwrap()
                    .id,
                "b"
            );
        }
    }

    #[test]
    fn unique_stale_ids_migrate_in_selection_order() {
        let endpoints = Endpoints {
            connected: vec![device("a", "A"), device("b", "B"), device("c", "C")],
            offline_ids: HashSet::new(),
        };
        let mut selected = vec![device("old-c", "C"), device("old-a", "A")];
        assert!(endpoints.reconcile(&mut selected));
        assert_eq!(selected, [device("c", "C"), device("a", "A")]);
        assert!(!endpoints.reconcile(&mut selected));
        assert_eq!(
            next_device(
                &endpoints.connected,
                &selected,
                &CycleMode::Available,
                Some("c")
            )
            .unwrap()
            .id,
            "a"
        );
    }

    #[test]
    fn exact_id_takes_priority_even_if_the_name_changed() {
        let endpoints = Endpoints {
            connected: vec![device("a", "Shared"), device("b", "B"), device("c", "C")],
            offline_ids: HashSet::new(),
        };
        let mut selected = vec![device("b", "Shared"), device("c", "C")];
        assert!(endpoints.reconcile(&mut selected));
        assert_eq!(selected[0], device("b", "B"));
        assert_eq!(
            next_device(
                &endpoints.connected,
                &selected,
                &CycleMode::Available,
                Some("c")
            )
            .unwrap()
            .id,
            "b"
        );
    }

    #[test]
    fn duplicate_names_and_known_offline_devices_keep_distinct_ids() {
        let endpoints = Endpoints {
            connected: vec![
                device("a", "Shared"),
                device("b", "Shared"),
                device("c", "One online"),
                device("d", "Has offline"),
            ],
            offline_ids: HashSet::from(["unplugged".into()]),
        };
        let saved = vec![
            device("old", "Shared"),
            device("a", "Shared"),
            device("b", "Shared"),
            device("old-c1", "One online"),
            device("old-c2", "One online"),
            device("unplugged", "Has offline"),
        ];
        let mut selected = saved.clone();
        assert!(!endpoints.reconcile(&mut selected));
        assert_eq!(selected, saved);
        assert_eq!(
            next_device(
                &endpoints.connected,
                &selected,
                &CycleMode::Available,
                Some("b")
            )
            .unwrap()
            .id,
            "a"
        );
    }
}
