# ── Connect view ────────────────────────────────────────────────────────
app-tagline = We have discord at home
nickname = Nickname
connect = Connect
settings = Settings
quit = Quit
error-nickname-empty = Nickname cannot be empty

# ── Missing-device warning ──────────────────────────────────────────────
device-missing-mic = mic "{ $name }"
device-missing-output = output "{ $name }"
device-warning = ⚠ Saved { $devices } not connected
device-warning-hint = Open Settings to pick another device
list-and = and

# ── Settings window ─────────────────────────────────────────────────────
settings-title = Settings
tab-general = General
tab-audio = Audio
tab-keyboard = Keyboard
tab-credits = Credits
tab-about = About
save-and-close = Save and Close
reset-defaults = Reset Defaults

# ── General panel ───────────────────────────────────────────────────────
language = Language
language-system = System default
language-note =
    Applies immediately. Windows that are already open update when
    reopened, and chat history keeps its original language.
# ── Server config (connect + settings) ──────────────────────────────────
server-password = Server password (optional)
settings-server = Server
tab-servers = Servers
server-name = Name
server-ip = IP / host
server-port = Port
server-default = Default
server-add = Add server
server-remove = Remove
server-manage = Manage servers
server-new-name = New server
save = Save
connect-access-denied = Access denied: wrong or missing server password — enter it below and Save, then Connect again
server-fingerprint = Server identity (SHA256:…, from the invite; learned on first connect)
server-config-note =
    The password is the room secret everyone on your server shares. The identity is learned on first connect and pinned; paste it from an invite to verify the server up front. Both apply on the next connect.

# ── Sidebar ─────────────────────────────────────────────────────────────
users-count = Users: { $count }
status-connecting = Connecting ...
status-connected = Connected
status-disconnected = Disconnected
mute = Mute
unmute = Unmute
deafen = Deafen
undeafen = Undeafen
share-screen = Share Screen
stop-sharing = Stop Sharing
disconnect = Disconnect
user-volume = User Volume
reset-audio = Reset Audio
watch-stream = Watch Stream
live = Live
stream-stats-quality = { $quality } · { $viewers } watching
stream-stats-frame = { $kb }KB · { $ms }ms enc
stream-stats-degraded = { $fps }% fps · loss { $loss }%
latency-line = Audio: { $audio } | Net: { $network } | ↑{ $up } ↓{ $down } Mb

# ── Chat ────────────────────────────────────────────────────────────────
send = Send
chat-disconnected = Disconnected: { $reason }
disconnect-reason-user = User disconnected
chat-user-joined = { $name } joined
chat-user-left = { $name } left
chat-share-started = Screen share started ({ $quality })
chat-share-stopped = Screen share stopped
chat-peer-sharing = { $name } started sharing their screen
chat-peer-stopped-sharing = { $name } stopped sharing their screen
chat-watching = Watching { $name }'s screen...
chat-stopped-watching = Stopped watching { $name }.
chat-mic-connected = Microphone connected: { $name }
chat-mic-disconnected = Microphone disconnected: { $name }
chat-speaker-connected = Speaker connected: { $name }
chat-speaker-disconnected = Speaker disconnected: { $name }
chat-input-lost = Input device lost — switching to system default
chat-input-default-changed = Default input changed to: { $name }
chat-output-lost = Output device lost — switching to system default
chat-output-default-changed = Default output changed to: { $name }
unknown-device = unknown device

# ── Tray & viewer ───────────────────────────────────────────────────────
tray-show = Show
tray-quit = Quit
viewer-title = { $name }'s screen ({ $width }x{ $height })
stop-watching = Stop Watching
viewer-stats = { $res } { $fps }fps { $kbps }KB/s { $mb }MB | buf { $depth }/{ $target }ms jit { $jitter }ms

# ── Audio panel ─────────────────────────────────────────────────────────
audio-devices-header = DEVICES
audio-input-device = Input Device (Microphone)
audio-output-device = Output Device (Speakers/Headphones)
audio-input-header = INPUT
audio-capture-level = Capture Level
audio-input-gain = Input Gain
audio-input-sensitivity = Input Sensitivity
audio-gate-hint = Audio below the red line won't be transmitted
audio-test-mic = Test Microphone
audio-stop-testing = Stop Testing
audio-test-hint = Listen to your own mic (others won't hear you)
audio-testing-hint = Listening to your mic... others can't hear you
audio-output-header = OUTPUT
audio-master-volume = Master Volume
audio-system-default = System Default
audio-device-count = { $inputs } input, { $outputs } output devices
audio-gain-value = Gain: { $percent }%
audio-volume-value = Volume: { $percent }%
audio-sensitivity-line = Sensitivity: { $name }  (threshold: { $threshold } RMS)
sensitivity-very-high = Very High
sensitivity-high = High
sensitivity-medium = Medium
sensitivity-low = Low
sensitivity-very-low = Very Low

# ── Keybind actions ─────────────────────────────────────────────────────
action-toggle-mute = Toggle Mute
action-toggle-deafen = Toggle Deafen
action-toggle-share = Toggle Screen Share
action-push-to-talk = Push to talk

# ── Keybinds panel ──────────────────────────────────────────────────────
keybinds-header = KEYBOARD SHORTCUTS
keybinds-hint = Click Record, then press a key combo. Esc cancels, Backspace clears.
keybind-not-set = Not set
keybind-record = Record
keybind-press-combo = Press a key combo...

# ── Screen share dialog ─────────────────────────────────────────────────
monitor = Monitor
monitor-primary = (primary)
quality = Quality
start-sharing = Start Sharing
error-no-monitors = No monitors found

# ── Credits panel ───────────────────────────────────────────────────────
credits-header = CREDITS
credits-super-testers = SUPER BETA TESTERS
credits-beta-testers = BETA TESTERS

# ── About panel ─────────────────────────────────────────────────────────
about-header = ABOUT
about-running-from = Running from:
about-config-file = Config file:
about-open-config = Open config location
about-open-folder = Open install folder
about-built-with = BUILT WITH
tech-rust = Rust - Programming Language
tech-fltk = FLTK - User Interface
tech-quinn = quinn-proto - QUIC transport
tech-ffmpeg = FFmpeg - Video Encoding/Decoding
tech-xcap = Xcap - Screen capture
tech-cpal = CPAL - Audio I/O
tech-opus = Opus - Audio Codec
tech-rnnoise = RNNoise - Noise Suppression
tech-rubato = Rubato - Audio Resampling

# ── Update flow ─────────────────────────────────────────────────────────
splash-checking = Checking for updates...
update-available-title = Update Available
update-available-heading = New version available!
update-version-info =
    Your version: { $current }
    Latest version: { $latest }
update-skip = Skip
update-now = Update
update-downloading-title = Updating
update-downloading-heading = Downloading update v{ $version }...
update-starting-download = Starting download...
update-progress = Downloading... { $percent }%
update-ready-title = Update Ready
update-downloaded-heading = Update downloaded!
update-ready-info = Ready to install. The app will restart automatically.
update-not-now = Not Now
# Rendered in an FLTK label — the code escapes '&' before display.
update-install-restart = Install & Restart
update-failed-title = Update Failed
update-failed-heading = Download failed
update-continue = Continue
cancel = Cancel

# ── Errors & shared ─────────────────────────────────────────────────────
error-invalid-address = Invalid address (use ip:port)
error-screen-share = Screen share: { $error }
fallback-username = User{ $id }
unknown = Unknown

# ── Chat input, image attach & context menus ────────────────────────────
chat-emoji-tooltip = Emoji — or type :name: codes like :joy:
chat-attach-tooltip = Send an image from your computer
chat-clear-tooltip = Clear chat (just for you)
chat-image-filter = Images
chat-image-load-error = Couldn't load image: { $error }
chat-image-undisplayable = { $name } sent an image that could not be displayed
image-preview-title = Send image
image-save-filter = PNG image
image-window-title = { $name } — image
menu-cut = Cut
menu-copy = Copy
menu-paste = Paste
menu-copy-message = Copy message
menu-copy-link = Copy link
menu-open-link = Open link
menu-open-image = Open image
menu-save-image = Save image…

# ── Resource stats (sidebar) ────────────────────────────────────────────
resource-line = CPU { $cpu }% · Mem { $mem } MB

# ── GIFs & media caps ───────────────────────────────────────────────────
chat-gif-too-large = GIF is over the { $mb } MB limit
chat-gif-too-big = Animated GIFs can be at most { $dim } px wide or tall
image-save-filter-gif = GIF image
chat-upload-progress = Uploading image… { $pct }%

# ── Slint-migration additions ───────────────────────────────────────────
tech-slint = Slint - User Interface
chat-image-too-large = Image is too large to send ({ $kb } KB after downscaling)
chat-image-save-error = Couldn't save image: { $error }
chat-downloading-image = Downloading image…

# ── Voice channels ──
channel-password-title = Channel password
channel-wrong-password = Wrong password — try again
channel-full = That channel is full
share-visibility = Visible to
share-channel-only = My channel
share-everyone = Everyone
chat-channels-updated = voice channels were updated

# Server identity pinning (1.0)
error-identity-changed = The server's identity changed: expected { $expected }, got { $observed }. Nothing was sent. If the admin rotated the key on purpose, edit the server's identity in Settings (or clear it) and connect again.
chat-identity-pinned = Pinned this server's identity: { $fingerprint }
