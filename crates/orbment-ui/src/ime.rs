use std::cell::Cell;
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    UI::{Input::Ime::*, WindowsAndMessaging::*},
};

#[derive(Clone, Debug)]
pub enum ImeInput {
    Enabled,
    Preedit(String),
    Commit(String),
    Disabled,
    Character(String),
}

/// Reusable for egui and native annotation editors. Coordinates are client physical pixels.
#[derive(Default)]
pub struct Imm32 {
    caret: Cell<RECT>,
    high_surrogate: Cell<Option<u16>>,
}

impl Imm32 {
    /// Preserve the native candidate window while the editor draws its own preedit.
    pub fn context_flags(flags: LPARAM) -> LPARAM {
        LPARAM(flags.0 & !(ISC_SHOWUICOMPOSITIONWINDOW as isize))
    }
    pub fn finish(&self, hwnd: HWND) {
        unsafe {
            let context = ImmGetContext(hwnd);
            if !context.is_invalid() {
                let _ = ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_COMPLETE, 0);
                let _ = ImmReleaseContext(hwnd, context);
            }
        }
    }

    pub fn set_caret(&self, hwnd: HWND, rect: RECT) {
        self.caret.set(rect);
        self.position(hwnd);
    }

    fn position(&self, hwnd: HWND) {
        unsafe {
            let context = ImmGetContext(hwnd);
            if context.is_invalid() {
                return;
            }
            let caret = self.caret.get();
            let point = POINT {
                x: caret.left,
                y: caret.bottom,
            };
            let _ = ImmSetCompositionWindow(
                context,
                &COMPOSITIONFORM {
                    dwStyle: CFS_POINT,
                    ptCurrentPos: point,
                    rcArea: caret,
                },
            );
            let _ = ImmSetCandidateWindow(
                context,
                &CANDIDATEFORM {
                    dwIndex: 0,
                    dwStyle: CFS_EXCLUDE,
                    ptCurrentPos: point,
                    rcArea: caret,
                },
            );
            let _ = ImmReleaseContext(hwnd, context);
        }
    }

    /// `Some` consumes the Win32 message, preventing DefWindowProc from duplicating committed text.
    pub fn handle(
        &self,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<Vec<ImeInput>> {
        match message {
            WM_IME_STARTCOMPOSITION => {
                self.position(hwnd);
                Some(vec![ImeInput::Enabled])
            }
            WM_IME_ENDCOMPOSITION => {
                Some(vec![ImeInput::Preedit(String::new()), ImeInput::Disabled])
            }
            WM_IME_COMPOSITION => {
                self.position(hwnd);
                let mut events = Vec::new();
                if lparam.0 as u32 & (GCS_COMPSTR.0 | GCS_RESULTSTR.0) == 0 {
                    events.push(ImeInput::Preedit(String::new()));
                }
                unsafe {
                    let context = ImmGetContext(hwnd);
                    if !context.is_invalid() {
                        for (flag, result) in [(GCS_RESULTSTR, true), (GCS_COMPSTR, false)] {
                            if lparam.0 as u32 & flag.0 == 0 {
                                continue;
                            }
                            let length = ImmGetCompositionStringW(context, flag, None, 0);
                            if length >= 0 {
                                let mut data = vec![0_u16; length as usize / 2];
                                let read = ImmGetCompositionStringW(
                                    context,
                                    flag,
                                    Some(data.as_mut_ptr().cast()),
                                    length as u32,
                                );
                                if read >= 0 {
                                    data.truncate(read as usize / 2);
                                    let text = String::from_utf16_lossy(&data);
                                    events.push(if result {
                                        ImeInput::Commit(text)
                                    } else {
                                        ImeInput::Preedit(text)
                                    });
                                }
                            }
                        }
                        let _ = ImmReleaseContext(hwnd, context);
                    }
                }
                Some(events)
            }
            WM_CHAR => {
                let unit = wparam.0 as u16;
                if (0xd800..=0xdbff).contains(&unit) {
                    self.high_surrogate.set(Some(unit));
                    return Some(Vec::new());
                }
                let text = if let Some(high) = self.high_surrogate.take() {
                    String::from_utf16_lossy(&[high, unit])
                } else {
                    String::from_utf16_lossy(&[unit])
                };
                Some(if text.chars().any(|ch| !ch.is_control()) {
                    vec![ImeInput::Character(text)]
                } else {
                    Vec::new()
                })
            }
            WM_KILLFOCUS => {
                self.high_surrogate.set(None);
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wm_char_combines_surrogates_and_discards_control_characters() {
        let ime = Imm32::default();
        let hwnd = HWND::default();
        assert!(
            ime.handle(hwnd, WM_CHAR, WPARAM(0xd834), LPARAM(0))
                .unwrap()
                .is_empty()
        );
        let result = ime
            .handle(hwnd, WM_CHAR, WPARAM(0xdd1e), LPARAM(0))
            .unwrap();
        assert!(matches!(result.as_slice(), [ImeInput::Character(text)] if text == "\u{1d11e}"));
        assert!(
            ime.handle(hwnd, WM_CHAR, WPARAM(13), LPARAM(0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn composition_end_clears_preedit_before_disabling_ime() {
        let result = Imm32::default()
            .handle(HWND::default(), WM_IME_ENDCOMPOSITION, WPARAM(0), LPARAM(0))
            .unwrap();
        assert!(
            matches!(result.as_slice(), [ImeInput::Preedit(text), ImeInput::Disabled] if text.is_empty())
        );
    }
}
