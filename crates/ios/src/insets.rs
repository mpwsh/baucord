//! Keeps the app clear of the status bar, the notch and the home
//! indicator (IOS-SPEC.md §14 D34, D39).
//!
//! Slint reads the window's safe area when winit reports a resize, and
//! winit reports one from its view's `layoutSubviews`. Two things went
//! wrong with that alone:
//!
//! - After a rotation the simulator kept the portrait insets in
//!   landscape — 62 pt at the top and 34 at the bottom, none at the
//!   sides: the name chip under the Dynamic Island, the stream page
//!   letterboxed into a band 96 pt short. The layout evidently runs
//!   before `UIKit` has given the window its new safe area, and nothing
//!   asks again (D34).
//! - On an iPhone SE the main page started right under the status bar:
//!   whatever Slint had, its top inset was 0 where `UIKit`'s is 20 pt
//!   (D39). Except that on the user's SE `UIKit` said 0 as well ("safe
//!   area: top 0 …" in the log, the status bar plainly on screen): a
//!   window on a phone without a notch need not count the status bar
//!   in its safe area. So the top is also at least the status bar's
//!   height while the scene shows one (`UIStatusBarManager`) — D46.
//!
//! So a probe — an empty, non-interactive view behind winit's, as large
//! as the window — is told `safeAreaInsetsDidChange` whenever the
//! window's safe area changes. It then does two things: asks winit's
//! view to lay out again (winit re-sends the resize and Slint re-reads
//! its insets), and hands `UIKit`'s own numbers to the app, which keeps
//! its pages clear of the larger of the two (`State.host-inset-*`).
//! The probe also reports from `layoutSubviews` — it is resized with
//! the window, so a rotation reaches it even when the safe area stays
//! put and only the status bar came or went. The numbers go to the log
//! once per change: "safe area: top 20 … (status bar 20)".

use std::{cell::Cell, ptr::NonNull};

use block2::RcBlock;
use objc2::{
    define_class, msg_send, rc::Retained, runtime::NSObject, MainThreadMarker, MainThreadOnly,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};
use objc2_ui_kit::{
    UIResponder, UIView, UIViewAutoresizing, UIWindow, UIWindowDidBecomeKeyNotification,
};

define_class!(
    // SAFETY: UIView may be subclassed; this one only overrides
    // `safeAreaInsetsDidChange` and `layoutSubviews`, calling UIView's
    // first, and has no `Drop`.
    #[unsafe(super(UIView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct SafeAreaProbe;

    impl SafeAreaProbe {
        #[unsafe(method(safeAreaInsetsDidChange))]
        fn safe_area_insets_did_change(&self) {
            // SAFETY: UIView's own implementation, as the docs ask.
            let _: () = unsafe { msg_send![super(self), safeAreaInsetsDidChange] };
            let window = self.window();
            relayout(window.as_deref());
            report(window.as_deref());
        }

        #[unsafe(method(layoutSubviews))]
        fn layout_subviews(&self) {
            // SAFETY: UIView's own implementation, as the docs ask.
            let _: () = unsafe { msg_send![super(self), layoutSubviews] };
            // Only the report: a changed number reaches Slint through
            // the app's own insets, and winit's view lays itself out
            // on the same pass.
            report(self.window().as_deref());
        }
    }
);

/// Ask winit's view (the window's root view) for a layout pass, which
/// makes winit send `Resized` and Slint re-read the safe area.
fn relayout(window: Option<&UIWindow>) {
    if let Some(view) = window
        .and_then(UIWindow::rootViewController)
        .and_then(|controller| controller.view())
    {
        view.setNeedsLayout();
    }
}

thread_local! {
    /// The probe is added to the first window that becomes key —
    /// winit's, the app's only one.
    static PROBED: Cell<bool> = const { Cell::new(false) };
    /// The safe area last reported (pt: top, left, bottom, right, as
    /// bits: compared for change, never for closeness).
    static REPORTED: Cell<Option<[u64; 4]>> = const { Cell::new(None) };
}

/// The status bar's height while the window's scene shows one, else 0.
fn status_bar_height(window: &UIWindow) -> f64 {
    window
        .windowScene()
        .and_then(|scene| scene.statusBarManager())
        .filter(|bar| !bar.isStatusBarHidden())
        .map_or(0.0, |bar| bar.statusBarFrame().size.height)
}

/// Hand the window's safe area to the app (and the log) when it
/// changed: `UIKit`'s, with the top raised to the status bar.
fn report(window: Option<&UIWindow>) {
    let Some(window) = window else {
        return;
    };
    let insets = window.safeAreaInsets();
    let status_bar = status_bar_height(window);
    let edges = [
        insets.top.max(status_bar),
        insets.left,
        insets.bottom,
        insets.right,
    ];
    let bits = edges.map(f64::to_bits);
    if REPORTED.replace(Some(bits)) == Some(bits) {
        return;
    }
    log::info!(
        "safe area: top {} left {} bottom {} right {} pt (status bar {status_bar})",
        edges[0],
        edges[1],
        edges[2],
        edges[3]
    );
    // Points are Slint's logical pixels (its scale factor is the
    // screen's).
    #[cfg(all(feature = "app", not(feature = "hello")))]
    frontend::app::set_host_insets(
        edges[0] as f32,
        edges[1] as f32,
        edges[2] as f32,
        edges[3] as f32,
    );
}

fn add_probe(window: &UIWindow, mtm: MainThreadMarker) {
    if PROBED.replace(true) {
        return;
    }
    let this = SafeAreaProbe::alloc(mtm).set_ivars(());
    // SAFETY: UIView's designated initialiser.
    let probe: Retained<SafeAreaProbe> =
        unsafe { msg_send![super(this), initWithFrame: window.bounds()] };
    probe.setUserInteractionEnabled(false);
    probe.setAutoresizingMask(
        UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
    );
    // Behind winit's view: covered, never drawn over the app.
    window.insertSubview_atIndex(&probe, 0);
    log::debug!("safe-area probe installed");
    // The window keeps the probe; and if the app started rotated, the
    // insets Slint has may already be stale.
    relayout(Some(window));
    report(Some(window));
}

/// Watch for winit's window; install before `frontend::app::run`.
pub fn install() {
    let center = NSNotificationCenter::defaultCenter();
    let queue = NSOperationQueue::mainQueue();
    let block = RcBlock::new(|note: NonNull<NSNotification>| {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // SAFETY: a valid notification for the block's duration.
        let note = unsafe { note.as_ref() };
        if let Some(window) = note.object().and_then(|o| o.downcast::<UIWindow>().ok()) {
            add_probe(&window, mtm);
        }
    });
    // SAFETY: the name is an immutable UIKit static; the block is
    // `'static` and runs on the main queue.
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(UIWindowDidBecomeKeyNotification),
            None,
            Some(&queue),
            &block,
        )
    };
    // Wanted for the whole process (one window, one probe).
    std::mem::forget(token);
}
