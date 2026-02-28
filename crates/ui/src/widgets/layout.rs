//! Layout primitives for panel construction.

use crate::theme;
use fltk::{enums, frame, group, prelude::*};

/// 1 px horizontal separator line.
pub fn separator(col: &mut group::Flex) {
    let mut sep = frame::Frame::default();
    sep.set_frame(enums::FrameType::FlatBox);
    sep.set_color(theme::SUBTLE);
    col.fixed(&sep, 1);
}
