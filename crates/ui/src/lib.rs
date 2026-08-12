//! Shared FLTK widgets and theming for voice-chat-core.
//!
//! This crate provides:
//!
//! - **[`theme`]** — one-call dark theme backed by [`fltk_theme`].
//! - **[`widgets`]** — reusable UI primitives: labels, controls, meters,
//!   layout helpers, and styling utilities.
//! - **[`fps`]** — frames-per-second measurement for streaming views.
//!
//! # Quick start
//!
//! ```no_run
//! let app = fltk::app::App::default();
//! ui::theme::apply();
//! // ... build your window ...
//! ```

#![deny(missing_docs)]

pub mod fps;
pub mod theme;
pub mod widgets;
