//! Keeps Slint's safe area right after a rotation (IOS-SPEC.md §14 D34).
//!
//! Slint reads the window's safe area (the notch or Dynamic Island, the
//! home indicator, the status bar) when winit reports a resize, and
//! winit reports one from its view's `layoutSubviews`. The simulator
//! showed the app keeping the portrait insets in landscape — 62 pt at
//! the top and 34 at the bottom, none at the sides: the name chip under
//! the Dynamic Island, the stream page letterboxed into a band 96 pt
//! short. So on a rotation that layout evidently runs before `UIKit`
//! has given the window its new safe area, and nothing asks again.
//!
//! The fix is a probe: an empty, non-interactive view behind winit's,
//! as large as the window. `UIKit` tells it `safeAreaInsetsDidChange`
//! whenever the window's safe area changes, and it then asks winit's
//! view to lay out again — winit re-sends the resize, and Slint reads
//! the insets afresh, now the new ones.

use std::cell::Cell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
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
            relayout(self.window().as_deref());
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
