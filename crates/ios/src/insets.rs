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
//!   (D39).
//!
//! So a probe — an empty, non-interactive view behind winit's, as large
//! as the window — is told `safeAreaInsetsDidChange` whenever the
//! window's safe area changes. It then does two things: asks winit's
//! view to lay out again (winit re-sends the resize and Slint re-reads
//! its insets), and hands `UIKit`'s own numbers to the app, which keeps
//! its pages clear of the larger of the two (`State.host-inset-*`).
//! The numbers go to the log once per change: "safe area: top 20 …".

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
    // `safeAreaInsetsDidChange`, calling UIView's first, and has no
    // `Drop`.
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

/// Hand the window's safe area to the app (and the log) when it
/// changed.
fn report(window: Option<&UIWindow>) {
    let Some(window) = window else {
        return;
    };
    let insets = window.safeAreaInsets();
    let edges = [insets.top, insets.left, insets.bottom, insets.right];
    let bits = edges.map(f64::to_bits);
    if REPORTED.replace(Some(bits)) == Some(bits) {
        return;
    }
    log::info!(
        "safe area: top {} left {} bottom {} right {} pt",
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
