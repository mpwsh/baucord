//! Reusable UI primitives for building panels.
//!
//! Organised into focused submodules:
//!
//! - **[`labels`]** — section headers, field labels, stat readouts, hints.
//! - **[`controls`]** — sliders, buttons, checkboxes, dropdowns.
//! - **[`meter`]** — VU level bars and threshold gate bars.
//! - **[`layout`]** — separators and spacing helpers.
//! - **[`style`]** — dark-mode styling for text inputs and displays.

pub mod controls;
pub mod labels;
pub mod layout;
pub mod meter;
pub mod style;
