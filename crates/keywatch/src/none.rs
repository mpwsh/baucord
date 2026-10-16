//! No key state to read here: every binding is registered instead.

use std::time::Duration;

use crate::{Interest, Kick, Modifier, Source, Woke};

pub(crate) fn is_modifier(_key: u16) -> bool {
    false
}

pub(crate) fn access_granted() -> bool {
    true
}

pub(crate) fn ask_for_access() {}

pub(crate) struct Keys;

pub(crate) fn open(_interest: Interest) -> Option<(Keys, Kick)> {
    None
}

impl Source for Keys {
    fn refresh(&mut self) -> bool {
        false
    }

    fn key_down(&self, _key: u16) -> bool {
        false
    }

    fn modifier_down(&self, _modifier: Modifier) -> bool {
        false
    }

    fn has_events(&self) -> bool {
        false
    }

    fn wait(&mut self, _timeout: Option<Duration>) -> Woke {
        Woke::Other
    }
}
