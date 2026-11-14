use anyhow::{Context, Result, bail, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, collections::BTreeMap, marker::PhantomData, rc::Rc};
use windows::Win32::{Foundation::HWND, UI::Input::KeyboardAndMouse::*};

static RECORDING: AtomicBool = AtomicBool::new(false);
type NativeBinding = (HWND, HOT_KEY_MODIFIERS, u32);
thread_local! {
    static NATIVE_BINDINGS: RefCell<BTreeMap<i32, NativeBinding>> = const { RefCell::new(BTreeMap::new()) };
}

pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

pub(crate) fn set_recording(recording: bool) {
    if RECORDING.swap(recording, Ordering::Relaxed) == recording {
        return;
    }
    NATIVE_BINDINGS.with(|bindings| {
        for (&id, &(hwnd, modifiers, key)) in bindings.borrow().iter() {
            let result = unsafe {
                if recording {
                    UnregisterHotKey(Some(hwnd), id)
                } else {
                    RegisterHotKey(Some(hwnd), id, modifiers, key)
                }
            };
            if let Err(error) = result {
                tracing::warn!(%error, id, recording, "Changing hotkey recording suspension failed");
            }
        }
    });
}

thread_local! {
    static REGISTRATIONS: RefCell<BTreeMap<i32, i32>> = const { RefCell::new(BTreeMap::new()) };
}

pub(crate) fn logical_id(id: i32) -> i32 {
    REGISTRATIONS.with(|values| values.borrow().get(&id).copied().unwrap_or(id))
}

pub struct Hotkeys {
    hwnd: HWND,
    entries: BTreeMap<i32, (i32, HOT_KEY_MODIFIERS, u32)>,
    _main_thread: PhantomData<Rc<()>>,
}

impl Hotkeys {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            entries: BTreeMap::new(),
            _main_thread: PhantomData,
        }
    }

    /// Failed bindings retain their old chord without blocking unrelated updates.
    pub fn sync(&mut self, desired: &[(i32, &str)]) -> Vec<(i32, anyhow::Error)> {
        let hwnd = self.hwnd;
        self.sync_with(
            desired,
            |id, modifiers, key| {
                if !is_recording() {
                    unsafe { RegisterHotKey(Some(hwnd), id, modifiers, key) }
                        .context("RegisterHotKey failed")?;
                }
                NATIVE_BINDINGS.with(|bindings| {
                    bindings.borrow_mut().insert(id, (hwnd, modifiers, key));
                });
                Ok(())
            },
            |id| {
                NATIVE_BINDINGS.with(|bindings| {
                    bindings.borrow_mut().remove(&id);
                });
                if let Err(error) = unsafe { UnregisterHotKey(Some(hwnd), id) } {
                    tracing::error!(%error, id, "UnregisterHotKey failed");
                }
            },
        )
    }

    fn sync_with(
        &mut self,
        desired: &[(i32, &str)],
        mut register: impl FnMut(i32, HOT_KEY_MODIFIERS, u32) -> Result<()>,
        mut unregister: impl FnMut(i32),
    ) -> Vec<(i32, anyhow::Error)> {
        let mut parsed = BTreeMap::new();
        let mut chords = BTreeMap::new();
        let mut failures = BTreeMap::new();
        for &(id, chord) in desired {
            if chord.trim().is_empty() {
                continue;
            }
            match parse_chord(chord) {
                Ok((modifiers, key)) => {
                    if let Some(previous) = chords.insert((modifiers.0, key), id) {
                        for duplicate in [previous, id] {
                            failures.insert(
                                duplicate,
                                anyhow::anyhow!("Duplicate hotkey chord: {chord}"),
                            );
                        }
                    }
                    parsed.insert(id, (modifiers, key));
                }
                Err(error) => {
                    failures.insert(id, error.context(format!("Invalid hotkey {id}: {chord}")));
                }
            }
        }

        let mut next = BTreeMap::new();
        for (id, (modifiers, key)) in parsed {
            if failures.contains_key(&id) {
                continue;
            }
            // Chord swaps reuse native registrations and only change logical routing.
            let existing = self
                .entries
                .values()
                .find(|(_, old_modifiers, old_key)| *old_modifiers == modifiers && *old_key == key);
            let native_id = if let Some((native_id, _, _)) = existing {
                *native_id
            } else {
                let reserved = (|| -> Result<i32> {
                    let native_id = REGISTRATIONS
                        .with(|values| {
                            let values = values.borrow();
                            (1..0xc000).find(|candidate| !values.contains_key(candidate))
                        })
                        .context("No hotkey identifiers available")?;
                    register(native_id, modifiers, key)
                        .with_context(|| format!("Registering hotkey {id}"))?;
                    REGISTRATIONS.with(|values| {
                        values.borrow_mut().insert(native_id, id);
                    });
                    Ok(native_id)
                })();
                match reserved {
                    Ok(native_id) => native_id,
                    Err(error) => {
                        failures.insert(id, error);
                        continue;
                    }
                }
            };
            next.insert(id, (native_id, modifiers, key));
        }

        // A failed update still owns its old chord. Preserve dependent old bindings too,
        // instead of stealing that chord while committing an otherwise valid swap.
        loop {
            let blocked: Vec<_> = next
                .iter()
                .filter_map(|(id, (native_id, _, _))| {
                    self.entries.iter().find_map(|(owner, (old_native, _, _))| {
                        (*old_native == *native_id && failures.contains_key(owner)).then_some(*id)
                    })
                })
                .collect();
            if blocked.is_empty() {
                break;
            }
            for id in blocked {
                next.remove(&id);
                failures.insert(
                    id,
                    anyhow::anyhow!("Hotkey {id} conflicts with a retained binding"),
                );
            }
        }
        for id in failures.keys() {
            if let Some(entry) = self.entries.get(id) {
                next.insert(*id, *entry);
            }
        }
        for (native_id, _, _) in self.entries.values() {
            if !next.values().any(|(next_id, _, _)| next_id == native_id) {
                unregister(*native_id);
                REGISTRATIONS.with(|values| {
                    values.borrow_mut().remove(native_id);
                });
            }
        }
        REGISTRATIONS.with(|values| {
            let mut values = values.borrow_mut();
            for (id, (native_id, _, _)) in &next {
                values.insert(*native_id, *id);
            }
        });
        self.entries = next;
        failures.into_iter().collect()
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        for (native_id, _, _) in self.entries.values() {
            NATIVE_BINDINGS.with(|bindings| {
                bindings.borrow_mut().remove(native_id);
            });
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), *native_id);
            }
            REGISTRATIONS.with(|values| {
                values.borrow_mut().remove(native_id);
            });
        }
    }
}

pub(crate) fn parse_chord(chord: &str) -> Result<(HOT_KEY_MODIFIERS, u32)> {
    let mut modifiers = MOD_NOREPEAT;
    let mut key = None;
    for part in chord.split('+') {
        let part = part.trim().to_ascii_uppercase();
        let modifier = match part.as_str() {
            "CTRL" | "CONTROL" => Some(MOD_CONTROL),
            "ALT" => Some(MOD_ALT),
            "SHIFT" => Some(MOD_SHIFT),
            "WIN" | "WINDOWS" | "SUPER" => Some(MOD_WIN),
            _ => None,
        };
        if let Some(modifier) = modifier {
            ensure!(
                !modifiers.contains(modifier),
                "Duplicate modifier in {chord}"
            );
            modifiers |= modifier;
            continue;
        }
        ensure!(
            key.is_none(),
            "Hotkey must contain exactly one ordinary key: {chord}"
        );
        let value = match part.as_str() {
            "SPACE" => VK_SPACE.0 as u32,
            "TAB" => VK_TAB.0 as u32,
            "ENTER" | "RETURN" => VK_RETURN.0 as u32,
            "ESC" | "ESCAPE" => VK_ESCAPE.0 as u32,
            "BACKSPACE" => VK_BACK.0 as u32,
            "DELETE" | "DEL" => VK_DELETE.0 as u32,
            "INSERT" | "INS" => VK_INSERT.0 as u32,
            "HOME" => VK_HOME.0 as u32,
            "END" => VK_END.0 as u32,
            "PAGEUP" | "PGUP" => VK_PRIOR.0 as u32,
            "PAGEDOWN" | "PGDN" => VK_NEXT.0 as u32,
            "LEFT" => VK_LEFT.0 as u32,
            "RIGHT" => VK_RIGHT.0 as u32,
            "UP" => VK_UP.0 as u32,
            "DOWN" => VK_DOWN.0 as u32,
            "PRINTSCREEN" => VK_SNAPSHOT.0 as u32,
            "VOLUME_MUTE" | "VOLUMEMUTE" => VK_VOLUME_MUTE.0 as u32,
            "VOLUME_UP" | "VOLUMEUP" => VK_VOLUME_UP.0 as u32,
            "VOLUME_DOWN" | "VOLUMEDOWN" => VK_VOLUME_DOWN.0 as u32,
            "MINUS" | "-" => VK_OEM_MINUS.0 as u32,
            "EQUALS" | "=" => VK_OEM_PLUS.0 as u32,
            "BRACKETLEFT" | "OPENBRACKET" | "[" => VK_OEM_4.0 as u32,
            "BRACKETRIGHT" | "CLOSEBRACKET" | "]" => VK_OEM_6.0 as u32,
            "BACKSLASH" | "\\" => VK_OEM_5.0 as u32,
            "SEMICOLON" | ";" => VK_OEM_1.0 as u32,
            "QUOTE" | "'" => VK_OEM_7.0 as u32,
            "COMMA" | "," => VK_OEM_COMMA.0 as u32,
            "PERIOD" | "." => VK_OEM_PERIOD.0 as u32,
            "SLASH" | "/" => VK_OEM_2.0 as u32,
            "BACKTICK" | "`" => VK_OEM_3.0 as u32,
            _ if part.len() == 1 && part.as_bytes()[0].is_ascii_alphanumeric() => {
                part.as_bytes()[0] as u32
            }
            _ if part.starts_with('F') => {
                let number = part[1..].parse::<u32>().context("Invalid function key")?;
                ensure!(
                    (1..=24).contains(&number),
                    "Function key must be F1 through F24"
                );
                VK_F1.0 as u32 + number - 1
            }
            _ => bail!("Unsupported hotkey key: {part}"),
        };
        key = Some(value);
    }
    Ok((modifiers, key.context("Hotkey has no ordinary key")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_reuse_registrations_and_unchanged_sets_do_no_native_work() {
        let mut hotkeys = Hotkeys::new(HWND::default());
        assert!(
            hotkeys
                .sync_with(&[(1, "Ctrl+F1"), (2, "Ctrl+F2")], |_, _, _| Ok(()), |_| {})
                .is_empty()
        );
        let first = hotkeys.entries[&1].0;
        let second = hotkeys.entries[&2].0;
        for _ in 0..2 {
            assert!(
                hotkeys
                    .sync_with(
                        &[(1, "Ctrl+F2"), (2, "Ctrl+F1")],
                        |_, _, _| panic!("Existing chord must not be registered again"),
                        |_| panic!("Existing chord must not be unregistered"),
                    )
                    .is_empty()
            );
            assert_eq!(hotkeys.entries[&1].0, second);
            assert_eq!(hotkeys.entries[&2].0, first);
            assert_eq!(logical_id(first), 2);
            assert_eq!(logical_id(second), 1);
        }
        assert!(
            hotkeys
                .sync_with(&[], |_, _, _| unreachable!(), |_| {})
                .is_empty()
        );
    }

    #[test]
    fn a_startup_conflict_does_not_disable_other_hotkeys() {
        let mut hotkeys = Hotkeys::new(HWND::default());
        let failures = hotkeys.sync_with(
            &[(1, "Ctrl+F1"), (2, "Ctrl+F2")],
            |_, _, key| {
                ensure!(key != VK_F1.0 as u32, "Already registered");
                Ok(())
            },
            |_| {},
        );
        assert_eq!(failures.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1]);
        assert!(!hotkeys.entries.contains_key(&1));
        assert!(hotkeys.entries.contains_key(&2));
        assert!(
            hotkeys
                .sync_with(&[], |_, _, _| unreachable!(), |_| {})
                .is_empty()
        );
    }

    #[test]
    fn failed_update_preserves_only_its_dependent_bindings() {
        let mut hotkeys = Hotkeys::new(HWND::default());
        assert!(
            hotkeys
                .sync_with(
                    &[(1, "Ctrl+F1"), (2, "Ctrl+F2"), (3, "Ctrl+F3")],
                    |_, _, _| Ok(()),
                    |_| {},
                )
                .is_empty()
        );
        let old = hotkeys.entries.clone();
        let failures = hotkeys.sync_with(
            &[
                (1, "Ctrl+F4"),
                (2, "Ctrl+F1"),
                (3, "Ctrl+F2"),
                (4, "Ctrl+F5"),
            ],
            |_, _, key| {
                ensure!(key != VK_F4.0 as u32, "Already registered");
                Ok(())
            },
            |_| panic!("Retained chords must not be released"),
        );
        assert_eq!(
            failures.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        for (id, entry) in old {
            assert_eq!(hotkeys.entries[&id], entry);
            assert_eq!(logical_id(entry.0), id);
        }
        assert!(hotkeys.entries.contains_key(&4));
        assert!(
            hotkeys
                .sync_with(&[], |_, _, _| unreachable!(), |_| {})
                .is_empty()
        );
    }

    #[test]
    fn invalid_chords_do_not_block_valid_changes_or_removals() {
        let mut hotkeys = Hotkeys::new(HWND::default());
        assert!(
            hotkeys
                .sync_with(&[(1, "Ctrl+F1"), (2, "Ctrl+F2")], |_, _, _| Ok(()), |_| {})
                .is_empty()
        );
        let old = hotkeys.entries[&1];
        let failures = hotkeys.sync_with(
            &[(1, "Ctrl+Invalid"), (3, "Ctrl+F3")],
            |_, _, _| Ok(()),
            |_| {},
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(hotkeys.entries[&1], old);
        assert!(!hotkeys.entries.contains_key(&2));
        assert!(hotkeys.entries.contains_key(&3));
        assert!(
            hotkeys
                .sync_with(&[], |_, _, _| unreachable!(), |_| {})
                .is_empty()
        );
    }
    #[test]
    fn recorded_punctuation_chords_map_to_windows_virtual_keys() {
        for (name, expected) in [
            ("Minus", VK_OEM_MINUS),
            ("Equals", VK_OEM_PLUS),
            ("BracketLeft", VK_OEM_4),
            ("BracketRight", VK_OEM_6),
            ("Backslash", VK_OEM_5),
            ("Semicolon", VK_OEM_1),
            ("Quote", VK_OEM_7),
            ("Comma", VK_OEM_COMMA),
            ("Period", VK_OEM_PERIOD),
            ("Slash", VK_OEM_2),
            ("Backtick", VK_OEM_3),
        ] {
            let (modifiers, key) = parse_chord(&format!("Ctrl+Alt+{name}")).unwrap();
            assert!(modifiers.contains(MOD_CONTROL | MOD_ALT));
            assert_eq!(key, expected.0 as u32);
        }
    }
}

thread_local! {
    static CTRL_PRESSES: RefCell<Vec<std::time::Instant>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn record_ctrl_press() {
    if is_recording() {
        CTRL_PRESSES.with(|presses| presses.borrow_mut().push(std::time::Instant::now()));
    }
}

pub(crate) fn cancel_recorder(context: &egui::Context) {
    context.data_mut(|data| data.remove::<egui::Id>(egui::Id::new("hotkey-recorder-active")));
    CTRL_PRESSES.with(|presses| presses.borrow_mut().clear());
    set_recording(false);
}

pub(crate) fn hotkey_recorder(ui: &mut egui::Ui, path: &str, chord: &mut String) -> bool {
    use crate::theme;
    use egui::{Key, RichText};
    let owner = egui::Id::new("hotkey-recorder-active");
    let id = egui::Id::new(("hotkey-recorder", path));
    let active = is_recording() && ui.data(|data| data.get_temp::<egui::Id>(owner)) == Some(id);
    let mut changed = false;
    if theme::icon_button(ui, theme::IconKind::Trash, theme::TEXT_MUTED, false)
        .on_hover_text("清除快捷键")
        .clicked()
    {
        chord.clear();
        changed = true;
        cancel_recorder(ui.ctx());
    }
    let start = ui
        .add(theme::small_button(if active {
            "取消"
        } else {
            "录入"
        }))
        .clicked();
    let caption = if active {
        "请按下快捷键… Esc 取消 / Backspace 清空".into()
    } else if chord.is_empty() {
        "点击录入快捷键".into()
    } else {
        chord.replace('+', " + ")
    };
    let response = ui.add(
        egui::Button::new(RichText::new(caption).size(13.0).color(if active {
            theme::ACCENT_LIGHT
        } else if chord.is_empty() {
            theme::TEXT_FAINT
        } else {
            theme::TEXT
        }))
        .fill(if active {
            theme::ACCENT_SOFT
        } else {
            theme::CONTROL
        })
        .stroke(theme::stroke(
            1.0,
            if active {
                theme::ACCENT
            } else {
                theme::CONTROL_BORDER
            },
        ))
        .corner_radius(6)
        .min_size(egui::vec2(160.0, 30.0)),
    );
    if start || response.clicked() {
        if active {
            cancel_recorder(ui.ctx());
        } else {
            cancel_recorder(ui.ctx());
            set_recording(true);
            ui.data_mut(|data| {
                data.insert_temp(owner, id);
                data.remove::<std::time::Instant>(id.with("ctrl"));
            });
            response.request_focus();
        }
        return changed;
    }
    if changed {
        return true;
    }
    if active && ui.input(|input| input.pointer.any_pressed()) && !response.hovered() {
        cancel_recorder(ui.ctx());
        return false;
    }
    if !active {
        return changed;
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(16));
    let events = ui.input(|input| input.events.clone());
    // Text events also allow accessibility/automation clients to supply a complete chord.
    let supplied = events.iter().find_map(|event| match event {
        egui::Event::Text(text) if text.chars().count() != 1 => Some(text.clone()),
        egui::Event::Paste(text) => Some(text.clone()),
        _ => None,
    });
    let mut commit = supplied
        .as_ref()
        .filter(|text| parse_chord(text).is_ok())
        .cloned();
    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        if key == Key::Escape {
            cancel_recorder(ui.ctx());
            return changed;
        }
        if key == Key::Backspace {
            commit = Some(String::new());
            break;
        }
        if commit.is_none() && supplied.is_none() {
            let name = match key {
                Key::ArrowUp => "Up".into(),
                Key::ArrowDown => "Down".into(),
                Key::ArrowLeft => "Left".into(),
                Key::ArrowRight => "Right".into(),
                Key::OpenBracket => "BracketLeft".into(),
                Key::CloseBracket => "BracketRight".into(),
                Key::Backtick => "Backtick".into(),
                Key::Minus => "Minus".into(),
                Key::Equals => "Equals".into(),
                Key::Backslash => "Backslash".into(),
                Key::Semicolon => "Semicolon".into(),
                Key::Quote => "Quote".into(),
                Key::Comma => "Comma".into(),
                Key::Period => "Period".into(),
                Key::Slash => "Slash".into(),
                _ => key.name().to_owned(),
            };
            let mut parts = Vec::new();
            if modifiers.ctrl {
                parts.push("Ctrl".to_owned());
            }
            if modifiers.alt {
                parts.push("Alt".to_owned());
            }
            if modifiers.shift {
                parts.push("Shift".to_owned());
            }
            if modifiers.mac_cmd {
                parts.push("Win".to_owned());
            }
            parts.push(name);
            let candidate = parts.join("+");
            if parse_chord(&candidate).is_ok() {
                commit = Some(candidate);
            }
        }
    }
    let mut presses = CTRL_PRESSES.with(|presses| std::mem::take(&mut *presses.borrow_mut()));
    let ctrl = ui.input(|input| {
        input.modifiers.ctrl
            && !input.modifiers.alt
            && !input.modifiers.shift
            && !input.modifiers.mac_cmd
    });
    let previous_ctrl =
        ui.data(|data| data.get_temp::<bool>(id.with("ctrl-down")).unwrap_or(false));
    ui.data_mut(|data| data.insert_temp(id.with("ctrl-down"), ctrl));
    if ctrl && !previous_ctrl && presses.is_empty() {
        presses.push(std::time::Instant::now());
    }
    if path == "file_search.hotkey" {
        for press in presses {
            let previous = ui.data(|data| data.get_temp::<std::time::Instant>(id.with("ctrl")));
            if previous.is_some_and(|previous| press.duration_since(previous).as_millis() <= 500) {
                commit = Some("DoubleCtrl".into());
            }
            ui.data_mut(|data| data.insert_temp(id.with("ctrl"), press));
        }
    }
    if let Some(next) = commit {
        *chord = next;
        changed = true;
        cancel_recorder(ui.ctx());
        response.surrender_focus();
    }
    changed
}
