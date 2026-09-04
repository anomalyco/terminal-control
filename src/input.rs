//! Shared wire key vocabulary. Adapters keep their own input envelopes and pacing.

use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Key {
    Enter,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Escape => b"\x1b",
            Self::ArrowUp => b"\x1b[A",
            Self::ArrowDown => b"\x1b[B",
            Self::ArrowLeft => b"\x1b[D",
            Self::ArrowRight => b"\x1b[C",
            Self::Tab => b"\t",
            Self::ShiftTab => b"\x1b[Z",
            Self::Backspace => b"\x7f",
            Self::Delete => b"\x1b[3~",
            Self::Home => b"\x1b[H",
            Self::End => b"\x1b[F",
            Self::PageUp => b"\x1b[5~",
            Self::PageDown => b"\x1b[6~",
        }
    }
}

#[cfg(test)]
pub(crate) const KEY_CASES: &[(&str, &[u8])] = &[
    ("enter", b"\r"),
    ("escape", b"\x1b"),
    ("arrowUp", b"\x1b[A"),
    ("arrowDown", b"\x1b[B"),
    ("arrowLeft", b"\x1b[D"),
    ("arrowRight", b"\x1b[C"),
    ("tab", b"\t"),
    ("shiftTab", b"\x1b[Z"),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
    ("pageUp", b"\x1b[5~"),
    ("pageDown", b"\x1b[6~"),
];
