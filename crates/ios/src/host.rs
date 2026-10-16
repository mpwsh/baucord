//! The frontend's file and link hooks on iOS (`frontend::host::Hooks`;
//! IOS-SPEC.md §10, §14 D31) — Android's `host.rs`, with `UIKit`'s
//! pickers in place of the Java ones, all in Rust through `objc2`:
//!
//! - **Attach** (`pick_file`): an action sheet asks *Photos* or
//!   *Files*; Photos is `PhotosUI`'s picker (out of process, so no
//!   permission prompt), Files the document picker (`asCopy`). An
//!   image-only pick (the avatar) goes straight to Photos. The chosen
//!   item is copied into `tmp/picks/` and that path handed back — the
//!   desktop dialog's road from there on (`attach_path`).
//! - **Save** (`save_download`): into `Documents/Downloads`, which the
//!   Files app shows as "On My iPhone › baucord › Downloads".
//! - **Open** (`open_target`): Quick Look through
//!   `UIDocumentInteractionController`; a type it cannot preview gets
//!   the share sheet instead ("Open in…", Save to Files, `AirDrop`).
//! - **Links** (`open_url`): `UIApplication openURL:`, i.e. Safari.
//! - **Copy** (`copy_text`): the general pasteboard.
//!
//! Everything `UIKit` runs on the main thread (Slint's), where the hooks
//! are called from. Photos hands its file over on a background queue;
//! the pick's callback is `Send` and hops back to the UI itself.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    ptr::NonNull,
};

use app_core::files::{safe_name, write_new, PickSlot};
use block2::RcBlock;
use frontend::host::{ChimeRoute, Hooks, KeepAlive, Notice, NoticeKind, OnPick, Saved};
use i18n::t;
use objc2::{
    define_class, extern_class, extern_methods, msg_send,
    rc::{Allocated, Retained},
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
    DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString, NSURL};
use objc2_photos_ui::{
    PHPickerConfiguration, PHPickerConfigurationAssetRepresentationMode, PHPickerFilter,
    PHPickerResult, PHPickerViewControllerDelegate,
};
use objc2_ui_kit::{
    UIAdaptivePresentationControllerDelegate, UIAlertAction, UIAlertActionStyle, UIAlertController,
    UIAlertControllerStyle, UIApplication, UIDocumentInteractionController,
    UIDocumentInteractionControllerDelegate, UIDocumentPickerDelegate,
    UIDocumentPickerViewController, UIPasteboard, UIPresentationController, UIResponder,
    UIViewController, UIWindowScene,
};
use objc2_uniform_type_identifiers::{UTType, UTTypeItem};

/// The pick in flight. One at a time, as on Android — a second Attach
/// while the first file is still being copied would clear the folder
/// under it. The callback is *claimed* by the first delivery of a file
/// ([`claim`]); the slot stays taken until that file is handed over or
/// has failed. A pick older than `app_core::files::PICK_STALE` is
/// taken as lost and no longer blocks a new one.
static PICK: PickSlot<OnPick> = PickSlot::new();

/// The waiting callback, for a file just delivered — once. `None`: the
/// pick was cancelled, or this is a second delivery of the same pick.
/// The simulator reported one Files pick twice (2026-10-06, IOS-SPEC.md
/// D36): the first moved the file and attached it, the second found it
/// gone and showed "No such file or directory".
fn claim() -> Option<OnPick> {
    PICK.claim()
}

/// Nothing was picked: free the slot — unless a file already delivered
/// is still being copied, which ends the pick itself.
fn cancel(how: &str) {
    if PICK.cancel() {
        log::info!("file pick cancelled{how}");
    }
}

/// Photos' type identifiers in the order a pick asks for them: what the
/// other platforms can show inline first (the picker transcodes HEIC
/// photos to JPEG in its *compatible* mode), then video, then any image
/// or movie at all (a HEIC that was not transcoded goes as a file card).
/// Never the item's first registered type blindly: for a Live Photo
/// that is the bundle, which arrives as a folder.
const PHOTO_TYPES: [&str; 7] = [
    "public.jpeg",
    "public.png",
    "com.compuserve.gif",
    "public.mpeg-4",
    "com.apple.quicktime-movie",
    "public.image",
    "public.movie",
];

struct IosHost {
    /// The app's `Documents` (the Files app's view of it).
    documents: Option<PathBuf>,
}

impl Hooks for IosHost {
    fn pick_file(&self, mime: &str, max_bytes: u64, on_pick: OnPick) {
        if PICK.begin(on_pick).is_err() {
            frontend::host::pick_failed(String::new(), "busy".to_string());
            return;
        }
        let images_only = mime.starts_with("image/");
        // Hooks run on Slint's thread, the main one; a call from
        // anywhere else is moved there.
        if let Some(mtm) = MainThreadMarker::new() {
            ask_where(mtm, images_only, max_bytes);
        } else if slint::invoke_from_event_loop(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                ask_where(mtm, images_only, max_bytes);
            }
        })
        .is_err()
        {
            PICK.finish();
        }
    }

    fn save_download(&self, name: &str, _mime: &str, bytes: &[u8]) -> Result<Saved, String> {
        let dir = self
            .documents
            .as_ref()
            .ok_or("the app has no Documents folder")?
            .join("Downloads");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let target = write_new(&dir, &safe_name(name), bytes).map_err(|e| e.to_string())?;
        let file = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        log::info!("saved {}", target.display());
        Ok(Saved {
            target,
            // What the Files app shows: On My iPhone › baucord › Downloads.
            shown: format!("{} › baucord › Downloads › {file}", t!("pick-from-files")),
        })
    }

    fn open_target(&self, target: &Path, _mime: &str) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        preview(mtm, target)
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        let url = NSURL::URLWithString(&NSString::from_str(url)).ok_or("not a valid link")?;
        let options = NSDictionary::new();
        // SAFETY: an empty options dictionary of the declared key and
        // value types; no completion handler.
        unsafe {
            UIApplication::sharedApplication(mtm)
                .openURL_options_completionHandler(&url, &options, None);
        }
        Ok(())
    }

    /// Connected: the moment to ask for notifications (D50) — the
    /// process itself needs nothing. In voice the audio background mode
    /// keeps it running (spec D13); chat alone is suspended with the app
    /// and reconnects on return.
    fn keep_alive(&self, notice: Option<&KeepAlive>) {
        if notice.is_some() {
            crate::notices::request_permission();
        }
    }

    // Notifications (D50): `notices.rs`.
    fn notify(&self, notice: &Notice) {
        crate::notices::post(notice);
    }

    fn notices_allowed(&self, _kind: NoticeKind) -> bool {
        crate::notices::allowed()
    }

    fn cancel_notice(&self, key: &str) {
        crate::notices::cancel(key);
    }

    /// An alert chime goes through the system, which honours the
    /// ring/silent switch (the voice session's audio would not) — while
    /// the user lets notifications sound; else through the engine.
    fn chime_route(&self) -> ChimeRoute {
        crate::notices::chime_route()
    }

    /// While a stream is watched the idle timer is off, so the screen
    /// neither dims nor locks (spec §14 D45). iOS applies it only while
    /// the app is in front: leaving the app lets the phone sleep.
    fn keep_screen_on(&self, on: bool) {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("keep screen on: not on the main thread");
            return;
        };
        UIApplication::sharedApplication(mtm).setIdleTimerDisabled(on);
    }

    /// The general pasteboard, as Slint's own text fields use it.
    fn copy_text(&self, text: &str) -> Result<(), String> {
        MainThreadMarker::new().ok_or("not on the main thread")?;
        let pasteboard = UIPasteboard::generalPasteboard();
        // SAFETY: on the main thread (checked above), where UIKit wants
        // the pasteboard used; a valid NSString.
        unsafe { pasteboard.setString(Some(&NSString::from_str(text))) };
        Ok(())
    }
}

/// Hand the frontend its hooks; before `frontend::app::run`.
pub fn install(documents: Option<PathBuf>) {
    frontend::host::install(Box::new(IosHost { documents }));
}

// ── Choosing where from ─────────────────────────────────────────────

/// An image-only pick goes to Photos; anything else asks first.
fn ask_where(mtm: MainThreadMarker, images_only: bool, max_bytes: u64) {
    if images_only {
        present_photos(mtm, true, max_bytes);
        return;
    }
    let Some(top) = top_view_controller(mtm) else {
        fail_pick(String::new(), "no window to show the picker in".to_string());
        return;
    };
    let sheet = UIAlertController::alertControllerWithTitle_message_preferredStyle(
        None,
        None,
        UIAlertControllerStyle::ActionSheet,
        mtm,
    );
    // UIKit calls an action's handler once the sheet has gone, so the
    // picker can be presented from there.
    let photos = RcBlock::new(move |_: NonNull<UIAlertAction>| {
        present_photos(mtm, false, max_bytes);
    });
    let files = RcBlock::new(move |_: NonNull<UIAlertAction>| {
        present_files(mtm, max_bytes);
    });
    let cancelled = RcBlock::new(|_: NonNull<UIAlertAction>| cancel(""));
    for (title, style, handler) in [
        (t!("pick-from-photos"), UIAlertActionStyle::Default, &photos),
        (t!("pick-from-files"), UIAlertActionStyle::Default, &files),
        (t!("file-cancel"), UIAlertActionStyle::Cancel, &cancelled),
    ] {
        let action = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str(&title)),
            style,
            Some(handler),
            mtm,
        );
        sheet.addAction(&action);
    }
    top.presentViewController_animated_completion(&sheet, true, None);
}

/// The view controller to present from: the root of the app's window,
/// or whatever it is already presenting (not one on its way out).
fn top_view_controller(mtm: MainThreadMarker) -> Option<Retained<UIViewController>> {
    let scenes = UIApplication::sharedApplication(mtm)
        .connectedScenes()
        .allObjects();
    let root = (0..scenes.count()).find_map(|i| {
        let scene = scenes.objectAtIndex(i).downcast::<UIWindowScene>().ok()?;
        let window = scene
            .keyWindow()
            .or_else(|| scene.windows().firstObject())?;
        window.rootViewController()
    })?;
    let mut top = root;
    while let Some(next) = top.presentedViewController() {
        if next.isBeingDismissed() {
            break;
        }
        top = next;
    }
    Some(top)
}

/// Both pickers are sheets a swipe down closes, and then neither tells
/// its own delegate (Apple DTS: no `picker:didFinishPicking:`, no
/// `documentPickerWasCancelled:`); only the sheet's presentation
/// controller does. Without this the slot stays taken and every Attach
/// says "still reading" for `PICK_STALE`.
fn watch_swipe(
    picker: &UIViewController,
    delegate: &ProtocolObject<dyn UIAdaptivePresentationControllerDelegate>,
) {
    if let Some(presentation) = picker.presentationController() {
        // SAFETY: a weak property; the delegate is kept alive in `KEEP`.
        unsafe { presentation.setDelegate(Some(delegate)) };
    }
}

/// `presentationControllerDidDismiss:` — sent only for a dismissal the
/// user made (the swipe), never for the picker's own dismissal after a
/// pick, so it cannot take a slot a pick is still filling.
fn swiped_away() {
    cancel(" (picker swiped away)");
}

/// The pick failed before any file: say so, and free the slot.
fn fail_pick(name: String, error: String) {
    log::warn!("file pick failed: {error}");
    PICK.finish();
    frontend::host::pick_failed(name, error);
}

/// The picked file is at `path`: free the slot and hand it over. (The
/// slot's lock is not held through the callback, which may decode and
/// write a picture — the avatar's.)
fn finish_pick(on_pick: OnPick, path: PathBuf) {
    PICK.finish();
    on_pick(path);
}

// ── Photos (PhotosUI) ───────────────────────────────────────────────

extern_class!(
    /// PhotosUI's picker. `objc2-photos-ui` 0.3.2 declares it for macOS
    /// only (it has no UIKit dependency), so the iOS class — a
    /// `UIViewController` — is declared here.
    #[unsafe(super(UIViewController, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct PHPickerViewController;
);

impl PHPickerViewController {
    extern_methods!(
        #[unsafe(method(initWithConfiguration:))]
        #[unsafe(method_family = init)]
        fn init_with_configuration(
            this: Allocated<Self>,
            configuration: &PHPickerConfiguration,
        ) -> Retained<Self>;

        /// Weak, as delegates are: the delegate is kept in [`KEEP`].
        #[unsafe(method(setDelegate:))]
        #[unsafe(method_family = none)]
        fn set_delegate(
            &self,
            delegate: Option<&ProtocolObject<dyn PHPickerViewControllerDelegate>>,
        );
    );
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = u64]
    struct PhotosDelegate;

    unsafe impl NSObjectProtocol for PhotosDelegate {}

    // The bindings declare the protocol's one (required) method for
    // macOS only; it is implemented here under the protocol all the
    // same, which is where objc2's debug check looks for it.
    unsafe impl PHPickerViewControllerDelegate for PhotosDelegate {
        #[unsafe(method(picker:didFinishPicking:))]
        fn picker_did_finish(
            &self,
            picker: &PHPickerViewController,
            results: &NSArray<PHPickerResult>,
        ) {
            // The picker does not close itself.
            picker.dismissViewControllerAnimated_completion(true, None);
            if let Some(result) = results.firstObject() {
                // SAFETY: a picker result's item provider.
                let provider = unsafe { result.itemProvider() };
                load_photo(&provider, *self.ivars());
            } else {
                cancel("");
            }
        }
    }

    unsafe impl UIAdaptivePresentationControllerDelegate for PhotosDelegate {
        #[unsafe(method(presentationControllerDidDismiss:))]
        fn did_dismiss(&self, _controller: &UIPresentationController) {
            swiped_away();
        }
    }
);

impl PhotosDelegate {
    fn new(mtm: MainThreadMarker, max_bytes: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(max_bytes);
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

fn present_photos(mtm: MainThreadMarker, images_only: bool, max_bytes: u64) {
    let Some(top) = top_view_controller(mtm) else {
        fail_pick(String::new(), "no window to show the picker in".to_string());
        return;
    };
    // SAFETY: plain property setters on a fresh configuration; the
    // filter is PhotosUI's own.
    let configuration = unsafe {
        let configuration = PHPickerConfiguration::new();
        configuration.setSelectionLimit(1);
        // JPEG for HEIC photos, H.264 for HEVC video: what the other
        // platforms decode.
        configuration.setPreferredAssetRepresentationMode(
            PHPickerConfigurationAssetRepresentationMode::Compatible,
        );
        if images_only {
            configuration.setFilter(Some(&PHPickerFilter::imagesFilter()));
        }
        configuration
    };
    let picker = PHPickerViewController::init_with_configuration(
        PHPickerViewController::alloc(mtm),
        &configuration,
    );
    let delegate = PhotosDelegate::new(mtm, max_bytes);
    picker.set_delegate(Some(ProtocolObject::from_ref(&*delegate)));
    watch_swipe(&picker, ProtocolObject::from_ref(&*delegate));
    KEEP.with_borrow_mut(|keep| keep.photos = Some(delegate));
    top.presentViewController_animated_completion(&picker, true, None);
}

/// Ask the item for a file in the first type of [`PHOTO_TYPES`] it
/// has (else its own first) and copy it out while it exists — Photos
/// deletes it when the completion handler returns.
fn load_photo(provider: &objc2_foundation::NSItemProvider, max_bytes: u64) {
    let registered = provider.registeredTypeIdentifiers();
    let wanted = PHOTO_TYPES
        .iter()
        .map(|t| NSString::from_str(t))
        .find(|t| provider.hasItemConformingToTypeIdentifier(t));
    let Some(type_id) = wanted else {
        log::warn!("photo pick: nothing usable among {registered:?}");
        fail_pick(String::new(), "not a photo or a video".to_string());
        return;
    };
    let stem = provider
        .suggestedName()
        .map_or_else(|| "photo".to_string(), |n| n.to_string());
    log::info!("photo pick: loading {type_id} for {stem} (has {registered:?})");
    let handler = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
        let Some(on_pick) = claim() else {
            log::info!("photo pick: delivered again or cancelled; ignored");
            return;
        };
        // SAFETY: the handler's arguments are valid or NULL.
        let (url, error) = unsafe { (url.as_ref(), error.as_ref()) };
        match (url, error) {
            (Some(url), _) => {
                let ext = url
                    .pathExtension()
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                let name = if ext.is_empty() {
                    stem.clone()
                } else {
                    format!("{stem}.{ext}")
                };
                match url.path() {
                    Some(path) => take_file(
                        Path::new(&path.to_string()),
                        &name,
                        max_bytes,
                        false,
                        on_pick,
                    ),
                    None => fail_pick(name, "the picked file has no path".to_string()),
                }
            }
            (None, Some(error)) => {
                fail_pick(stem.clone(), error.localizedDescription().to_string());
            }
            (None, None) => fail_pick(stem.clone(), "the photo could not be read".to_string()),
        }
    });
    // SAFETY: the block only touches `Send` data (strings, the pending
    // slot behind its mutex), so any queue may run it.
    let _progress = unsafe {
        provider.loadFileRepresentationForTypeIdentifier_completionHandler(&type_id, &handler)
    };
}

// ── Files (the document picker) ─────────────────────────────────────

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = u64]
    struct FilesDelegate;

    unsafe impl NSObjectProtocol for FilesDelegate {}

    unsafe impl UIDocumentPickerDelegate for FilesDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick_documents(
            &self,
            _controller: &UIDocumentPickerViewController,
            urls: &NSArray<NSURL>,
        ) {
            let max_bytes = *self.ivars();
            let Some(on_pick) = claim() else {
                log::info!("document picker: the pick was reported again; ignored");
                return;
            };
            let Some(url) = urls.firstObject() else {
                cancel("");
                return;
            };
            let name = url
                .lastPathComponent()
                .map(|n| n.to_string())
                .unwrap_or_default();
            let Some(path) = url.path().map(|p| PathBuf::from(p.to_string())) else {
                fail_pick(name, "the picked file has no path".to_string());
                return;
            };
            // `asCopy`: the file is already a copy in the app's tmp,
            // moved rather than copied again — off the main thread all
            // the same, since a move across volumes is a copy.
            std::thread::spawn(move || take_file(&path, &name, max_bytes, true, on_pick));
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _controller: &UIDocumentPickerViewController) {
            cancel("");
        }
    }

    unsafe impl UIAdaptivePresentationControllerDelegate for FilesDelegate {
        #[unsafe(method(presentationControllerDidDismiss:))]
        fn did_dismiss(&self, _controller: &UIPresentationController) {
            swiped_away();
        }
    }
);

impl FilesDelegate {
    fn new(mtm: MainThreadMarker, max_bytes: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(max_bytes);
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

fn present_files(mtm: MainThreadMarker, max_bytes: u64) {
    let Some(top) = top_view_controller(mtm) else {
        fail_pick(String::new(), "no window to show the picker in".to_string());
        return;
    };
    // SAFETY: an immutable UTType constant.
    let any: &UTType = unsafe { UTTypeItem };
    let types = NSArray::from_slice(&[any]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
        true,
    );
    let delegate = FilesDelegate::new(mtm, max_bytes);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    watch_swipe(&picker, ProtocolObject::from_ref(&*delegate));
    KEEP.with_borrow_mut(|keep| keep.files = Some(delegate));
    top.presentViewController_animated_completion(&picker, true, None);
}

// ── The picked file ─────────────────────────────────────────────────

/// Bring the picked file into `tmp/picks/` as `name` (moved when it is
/// already the app's own copy) and hand it over; too large or
/// unreadable, say so instead. Runs off the main thread.
fn take_file(source: &Path, name: &str, max_bytes: u64, owned: bool, on_pick: OnPick) {
    // The document picker's copy is the app's: not left behind in tmp
    // when the pick fails (a refused multi-GB video, say).
    let fail = |error: String| {
        if owned {
            let _ = std::fs::remove_file(source);
        }
        fail_pick(name.to_string(), error);
    };
    let size = match std::fs::metadata(source) {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return fail("not a file".to_string()),
        Err(e) => return fail(e.to_string()),
    };
    if size > max_bytes {
        return fail("too-large".to_string());
    }
    let dir = std::env::temp_dir().join("picks");
    // One pick at a time (`PICK`), and the frontend reads the file as
    // soon as it is handed over: the last pick's copy can go.
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(e.to_string());
    }
    let target = dir.join(safe_name(name));
    let moved = owned && std::fs::rename(source, &target).is_ok();
    if !moved && let Err(e) = std::fs::copy(source, &target) {
        return fail(e.to_string());
    }
    log::info!("picked {} ({size} bytes)", target.display());
    finish_pick(on_pick, target);
}

// ── Opening a saved file (Quick Look) ───────────────────────────────

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct PreviewDelegate;

    unsafe impl NSObjectProtocol for PreviewDelegate {}

    unsafe impl UIDocumentInteractionControllerDelegate for PreviewDelegate {
        #[unsafe(method_id(documentInteractionControllerViewControllerForPreview:))]
        fn view_controller_for_preview(
            &self,
            _controller: &UIDocumentInteractionController,
        ) -> Retained<UIViewController> {
            let mtm = self.mtm();
            top_view_controller(mtm).unwrap_or_else(|| UIViewController::new(mtm))
        }
    }
);

impl PreviewDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// Quick Look for `target`; the share sheet for a type it cannot show.
fn preview(mtm: MainThreadMarker, target: &Path) -> Result<(), String> {
    let path = target.to_str().ok_or("not a saved file")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let controller = UIDocumentInteractionController::interactionControllerWithURL(&url, mtm);
    let delegate = PreviewDelegate::new(mtm);
    controller.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // Neither is retained by UIKit while on screen.
    KEEP.with_borrow_mut(|keep| keep.preview = Some((controller.clone(), delegate)));
    if controller.presentPreviewAnimated(true) {
        return Ok(());
    }
    let view = top_view_controller(mtm)
        .and_then(|top| top.view())
        .ok_or("no window to show the file in")?;
    let bounds = view.bounds();
    let anchor = CGRect::new(
        CGPoint::new(bounds.size.width / 2.0, bounds.size.height / 2.0),
        CGSize::new(1.0, 1.0),
    );
    if controller.presentOptionsMenuFromRect_inView_animated(anchor, &view, true) {
        Ok(())
    } else {
        Err("no app on this iPhone opens this kind of file".to_string())
    }
}

// ── What UIKit holds weakly ─────────────────────────────────────────

/// Delegates are weak references in `UIKit`, and a document interaction
/// controller is not retained while it shows: the host keeps the
/// current ones alive here (main thread only), replacing them with the
/// next.
#[derive(Default)]
struct Keep {
    photos: Option<Retained<PhotosDelegate>>,
    files: Option<Retained<FilesDelegate>>,
    preview: Option<(
        Retained<UIDocumentInteractionController>,
        Retained<PreviewDelegate>,
    )>,
}

thread_local! {
    static KEEP: RefCell<Keep> = RefCell::new(Keep::default());
}
