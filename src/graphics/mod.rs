//! Kitty Graphics Protocol support (Unicode-placeholder model).
//!
//! Parses APC `_G…` (and DCS `tmux;` wrappers), stores image blobs, and
//! writes `U+10EEEE` placeholder cells so the grid can scroll/crop while
//! attached clients re-emit placements to a Kitty-capable outer TTY.

pub mod kitty;
pub mod placeholder;
pub mod store;

pub use kitty::GraphicsEvent;
pub use store::ImageStore;
