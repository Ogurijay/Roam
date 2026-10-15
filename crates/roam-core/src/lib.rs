//! Roam 平台无关的核心逻辑，可以在任何平台上测试。

pub mod config;
pub mod health;
pub mod keymap;
pub mod layout;
#[cfg(feature = "net")]
pub mod net;
pub mod router;
pub mod settings;
#[cfg(feature = "net")]
pub mod ticket;
