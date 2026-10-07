//! The app lifecycle `UIKit` reports, seen from Rust (IOS-SPEC.md §4.3).
//!
//! winit turns `UIKit`'s suspend/resume into its own events, but Slint
//! does not pass them on to the application, so the host listens to the
//! notifications itself. Background and foreground are the *scene's*:
//! the app adopts the scene life cycle (required to launch when built
//! with the iOS 27 SDK), and TN3187 moves those events to the scene —
//! the application's notifications are still documented, but the
//! scene's are the ones that life cycle is about. The memory warning
//! and termination stay the application's. They are
//! delivered on the main queue — the main thread, which is Slint's — so
//! the handlers can reach the app directly.
//!
//! What matters on iOS: the event loop never returns, and a suspended
//! app is ended without being told. Whatever must survive is written on
//! the way into the background.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSOperationQueue,
};
use objc2_ui_kit::{
    UIApplicationDidReceiveMemoryWarningNotification, UIApplicationWillTerminateNotification,
    UISceneDidEnterBackgroundNotification, UISceneWillEnterForegroundNotification,
};

/// Register the observers, for the life of the process.
pub fn install() {
    let center = NSNotificationCenter::defaultCenter();
    let queue = NSOperationQueue::mainQueue();
    // SAFETY: the notification names are immutable framework statics.
    let (background, foreground, memory, terminate) = unsafe {
        (
            UISceneDidEnterBackgroundNotification,
            UISceneWillEnterForegroundNotification,
            UIApplicationDidReceiveMemoryWarningNotification,
            UIApplicationWillTerminateNotification,
        )
    };

    observe(&center, &queue, background, || {
        log::info!("lifecycle: background");
        // The settings now: iOS may end the process from here without
        // another word (and `run` never returns to save them).
        frontend::app::persist_for_background();
        // In voice the process keeps running (the audio background
        // mode): what only feeds the screen stops, so the app does not
        // draw frames nobody sees (spec §14 D42).
        frontend::app::set_host_visible(false);
        log::logger().flush();
    });
    observe(&center, &queue, foreground, || {
        log::info!("lifecycle: foreground");
        // The user may have changed the notification settings meanwhile.
        crate::notices::refresh();
        frontend::app::set_host_visible(true);
    });
    observe(&center, &queue, memory, || {
        // app-core bounds its image caches already; worth knowing
        // when it happens all the same (spec §4.3).
        log::warn!("lifecycle: memory warning");
    });
    observe(&center, &queue, terminate, || {
        // Only sent to an app that is running, not suspended — rare,
        // but free.
        log::info!("lifecycle: terminating");
        frontend::app::persist_for_background();
        log::logger().flush();
    });
}

/// Run `handler` on the main queue whenever `name` is posted.
fn observe(
    center: &NSNotificationCenter,
    queue: &NSOperationQueue,
    name: &NSNotificationName,
    handler: impl Fn() + 'static,
) {
    let block = RcBlock::new(move |_note: NonNull<NSNotification>| handler());
    // SAFETY: the block is `'static` and is only called on the main
    // queue, which is where the app state lives.
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(name), None, Some(queue), &block)
    };
    // The observer is wanted for the whole process; dropping the token
    // would only lose the handle needed to remove it.
    std::mem::forget(token);
}
