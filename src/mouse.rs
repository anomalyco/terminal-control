//! Typed mouse input in zero-based terminal cells, independent of presentation.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Move,
    Down,
    Up,
    Click,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

impl From<Button> for libghostty_vt::mouse::Button {
    fn from(button: Button) -> Self {
        match button {
            Button::Left => Self::Left,
            Button::Middle => Self::Middle,
            Button::Right => Self::Right,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MouseEvent {
    pub action: Action,
    /// Zero-based column, within the current viewport.
    pub x: u16,
    /// Zero-based row, within the current viewport.
    pub y: u16,
    /// Button for click/down/up. Moves use the currently held button.
    #[serde(default)]
    pub button: Button,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub ctrl: bool,
}

impl MouseEvent {
    pub fn new(action: Action, x: u16, y: u16) -> Self {
        Self {
            action,
            x,
            y,
            button: Button::Left,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }
}
