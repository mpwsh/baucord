//! Shared localization for every baucord frontend.
//!
//! FTL sources are embedded at compile time (one binary, no loose
//! files). The API is a process-global localizer plus the [`t!`]
//! macro:
//!
//! ```no_run
//! use i18n::t;
//! i18n::init(None); // None = follow the OS locale
//! let label = t!("connect");
//! let warn = t!("device-warning", "devices" => "mic \"USB\"");
//! ```
//!
//! Lookup falls back per-message to English, then to the message id
//! itself (so a missing translation is visible, never a crash).

use std::sync::RwLock;

/// Re-export used by the `t!` macro expansion.
pub use fluent_bundle::FluentArgs as Args;
use fluent_bundle::{concurrent::FluentBundle, FluentArgs, FluentResource};
use unic_langid::LanguageIdentifier;

type Bundle = FluentBundle<FluentResource>;

/// A shippable locale.
pub struct Locale {
    /// BCP-47 code, also what settings.json stores.
    pub code: &'static str,
    /// Name shown in the language picker, in its own language.
    pub native_name: &'static str,
    ftl: &'static str,
}

/// Every embedded locale. First entry ("en") is the reference locale
/// and the fallback — it must define every message id.
pub const LOCALES: &[Locale] = &[
    Locale {
        code: "en",
        native_name: "English",
        ftl: include_str!("../locales/en.ftl"),
    },
    Locale {
        code: "es-AR",
        native_name: "Español (Argentina)",
        ftl: include_str!("../locales/es-AR.ftl"),
    },
    Locale {
        code: "pt-BR",
        native_name: "Português (Brasil)",
        ftl: include_str!("../locales/pt-BR.ftl"),
    },
];

struct State {
    current: Bundle,
    /// `None` when the current locale *is* English.
    fallback: Option<Bundle>,
    code: &'static str,
}

static STATE: RwLock<Option<State>> = RwLock::new(None);

/// Initialize with a preferred BCP-47 code, or `None` to follow the OS
/// locale. Returns the resolved locale code. Call before building UI.
#[must_use]
pub fn init(preferred: Option<&str>) -> &'static str {
    set_locale(preferred)
}

/// Switch locale at runtime. Already-constructed widgets keep their
/// labels — frontends re-set or rebuild per their own strategy.
///
/// # Panics
/// If the locale lock is poisoned, i.e. a translation panicked
/// while holding it — the bundles are read-only after construction,
/// so that cannot happen in practice.
pub fn set_locale(preferred: Option<&str>) -> &'static str {
    let requested = preferred.map(str::to_owned).or_else(sys_locale::get_locale);
    let locale = resolve(requested.as_deref());
    let fallback = (locale.code != "en").then(|| build_bundle(&LOCALES[0]));
    let state = State {
        current: build_bundle(locale),
        fallback,
        code: locale.code,
    };
    *STATE.write().unwrap() = Some(state);
    log::info!("i18n: locale set to {}", locale.code);
    locale.code
}

/// The active locale code ("en" before `init`).
///
/// # Panics
/// If the locale lock is poisoned (see [`set_locale`]).
pub fn current_locale() -> &'static str {
    STATE.read().unwrap().as_ref().map_or("en", |s| s.code)
}

/// Translate a message id. Prefer the [`t!`] macro.
///
/// # Panics
/// If the locale lock is poisoned (see [`set_locale`]).
pub fn translate(id: &str, args: Option<&FluentArgs>) -> String {
    let guard = STATE.read().unwrap();
    let Some(state) = guard.as_ref() else {
        return id.to_string();
    };
    lookup(&state.current, id, args)
        .or_else(|| state.fallback.as_ref().and_then(|b| lookup(b, id, args)))
        .unwrap_or_else(|| {
            log::warn!("i18n: missing message `{id}`");
            id.to_string()
        })
}

fn lookup(bundle: &Bundle, id: &str, args: Option<&FluentArgs>) -> Option<String> {
    let message = bundle.get_message(id)?;
    let pattern = message.value()?;
    let mut errors = Vec::new();
    let text = bundle.format_pattern(pattern, args, &mut errors);
    for err in &errors {
        log::warn!("i18n: format error in `{id}`: {err}");
    }
    Some(text.into_owned())
}

fn build_bundle(locale: &Locale) -> Bundle {
    let langid: LanguageIdentifier = locale
        .code
        .parse()
        .unwrap_or_else(|_| LanguageIdentifier::default());
    let mut bundle = Bundle::new_concurrent(vec![langid]);
    // Fluent wraps placeables in Unicode bidi isolation marks by
    // default; FLTK renders those as visible garbage, so keep them off.
    bundle.set_use_isolating(false);
    match FluentResource::try_new(locale.ftl.to_owned()) {
        Ok(resource) => {
            if let Err(errors) = bundle.add_resource(resource) {
                for err in &errors {
                    log::error!("i18n: duplicate message in {}: {err}", locale.code);
                }
            }
        }
        Err((resource, errors)) => {
            for err in &errors {
                log::error!("i18n: parse error in {}: {err}", locale.code);
            }
            bundle.add_resource_overriding(resource);
        }
    }
    bundle
}

/// Best match for a requested code: exact, then language subtag, then
/// English.
fn resolve(requested: Option<&str>) -> &'static Locale {
    let Some(requested) = requested else {
        return &LOCALES[0];
    };
    if let Some(exact) = LOCALES
        .iter()
        .find(|l| l.code.eq_ignore_ascii_case(requested))
    {
        return exact;
    }
    let lang = requested.split(['-', '_']).next().unwrap_or(requested);
    LOCALES
        .iter()
        .find(|l| {
            l.code
                .split('-')
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(lang))
        })
        .unwrap_or(&LOCALES[0])
}

/// Translate a message: `t!("id")` or
/// `t!("id", "key" => value, ...)`.
#[macro_export]
macro_rules! t {
    ($id:expr) => {
        $crate::translate($id, None)
    };
    ($id:expr, $($key:literal => $value:expr),+ $(,)?) => {{
        let mut args = $crate::Args::new();
        $( args.set($key, $value); )+
        $crate::translate($id, Some(&args))
    }};
}
