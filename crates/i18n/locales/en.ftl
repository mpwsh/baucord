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
invite-code = Invite code
invite-code-placeholder = e.g. K7FQ-2MX9-ABCD — only needed the first time
server-invite-note = Sent once, on your first visit; from then on this device's key is the login.
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
server-fingerprint = Server identity (SHA256:…, from the invite; learned on first connect)
server-config-note =
    The identity is learned on first connect and pinned; paste it from the server's startup log to verify the server up front. Applies on the next connect.

# ── Admission verdicts (connect card) ───────────────────────────────────
connect-need-invite = This server doesn't know this device yet. Enter the invite code you were given and press Connect.
connect-invite-expired = That invite code has expired — ask for a new one.
connect-invite-revoked = That invite code was revoked — ask for a new one.
connect-invite-exhausted = That invite code has been used up — ask for a new one.
connect-invite-unknown = That's not a valid invite code. Check it and try again.
connect-key-retired = This device was removed from your user on this server. Ask an admin to re-link it, or regenerate your identity in Settings to start over as a new user.
connect-name-in-use = That nickname is taken on this server right now — pick another.
error-invite-code-malformed = That doesn't look like an invite code or link.
error-no-identity = Couldn't load your identity key ({ $path }). Fix or delete the file and restart.
error-banned = You are banned from this server{ $until }{ $reason }
ban-for = for { $left }
ban-indefinite = indefinitely
removed-disconnected = Disconnected by { $by }: { $reason }
removed-kicked = Kicked by { $by }: { $reason } — you'll need a new invite to come back.
removed-banned = Banned by { $by }{ $until }{ $reason }
removed-replaced = You signed in from another device or connection; this one was closed.
chat-server-not-persistent = This server keeps nothing between restarts: roles, invites and bans last until it stops.

# ── Invites (/invite in the composer) ───────────────────────────────────
chat-invite-created = Invite code { $code } (copied) — { $role }, { $expiry }, { $uses }. Give it together with the server's address and port.
chat-invite-usage = Usage: /invite [guest|member|organizer] [24h|7d] [uses]
chat-invite-no-permission = You can't create that invite here.
chat-invite-role-not-allowed = Invite codes can't grant that role.
invite-uses = { $count } use(s)
invite-uses-unlimited = unlimited uses
invite-expires-in = expires in { $left }
invite-never-expires = never expires

# ── Moderation (slash commands; spec §4, §8) ────────────────────────────
chat-commands-help = Commands: { $commands }
chat-command-unknown = Unknown command /{ $command }. Try /help.
chat-command-usage = Usage: /{ $command } name …
chat-command-usage-revoke-invite = Usage: /revoke-invite <code>
chat-command-usage-unlink = Usage: /unlink <key hash, 64 hex digits> (see /users)
chat-command-usage-link = Usage: /link <name> <key hash, 64 hex digits>
chat-command-no-such-user = Nobody online is called { $name }.
chat-admin-refused = Refused: { $why }.
chat-admin-done = Done.
chat-server-muted-locked = You are muted by the server; you can unmute once it lifts.
chat-your-roles-changed = Your roles are now: { $roles }
roles-none = none
reason-none = (no reason given)
chat-admin-users-header = { $count } user(s) of record (● online):
chat-admin-restrictions-header = { $count } restriction(s):
chat-admin-invites-header = { $count } invite(s):
restricted-voice = You have been muted by the server{ $until }{ $reason }
restricted-text = You may not write in chat{ $until }{ $reason }
restricted-dm = You may not send direct messages{ $until }{ $reason }
restricted-upload = You may not upload files{ $until }{ $reason }
restricted-share = You may not share your screen{ $until }{ $reason }
restricted-profile = You may not change your profile{ $until }{ $reason }
restricted-server = You may not join{ $until }{ $reason }
lifted-voice = Your mute was lifted.
lifted-text = You may write in chat again.
lifted-dm = You may send direct messages again.
lifted-upload = You may upload files again.
lifted-share = You may share your screen again.
lifted-profile = You may change your profile again.
lifted-server = You may join again.
restricted-by-role = not allowed by your role

# ── Identity (Settings → General) ───────────────────────────────────────
identity-header = Identity
identity-export = Export
identity-import = Import
identity-regenerate = Regenerate
identity-note = This device's key is who you are on every server. Export it to be the same user on another machine; Import replaces this device's key; Regenerate makes a new one (a new user — you'll need an invite).
identity-missing = no identity key
identity-exported = Identity exported.
identity-imported = Identity imported — you are now this key. Reconnect to use it.
identity-regenerated = New identity created. The next connection needs an invite.

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

# Connection outcomes (1.0.1). Short, in the user's language; the
# technical reason is in baucord.log.
error-lost-timeout = Connection lost — no response from the server. Check your internet.
error-lost-reset = Connection lost — the server dropped this session after your connection went quiet.
error-lost-unreachable = Can't reach the server right now — your connection is losing packets.
error-lost-generic = Connection lost.
error-lost-gave-up = Automatic reconnect gave up; press Connect to try again.
error-reconnecting = Reconnecting in { $secs } s…
error-refused-full = The server is full right now.
error-refused-rate-limited = Too many failed attempts — wait a minute and try again.
error-outdated = This version of baucord doesn't match the server. Update baucord and try again.
error-local = Couldn't start the connection: { $detail }
error-lost-restart = The server is restarting.

# ── History, progress, channels (spec §5, §6, §9) ───────────────────────
chat-history-loaded = { $count } earlier message(s) shown{ $more ->
    [yes] ; /history loads more
   *[no] .
}
chat-history-unavailable = History is not available right now; try again in { $secs } s.
chat-edited-mark = (edited)
chat-message-edited = Message #{ $id } was edited: { $text }
chat-message-deleted = Message #{ $id } was deleted.
chat-rate-limited = Slow down — chat is rate limited; try again in { $secs } s.
chat-level-up = Level up! You are now level { $level } ({ $xp } xp).
chat-moved-idle = You were moved to the AFK channel after { $min } idle minutes.
chat-command-usage-channel = Usage: /channel list · /channel create <name> [password] · /channel keep <name> · /channel delete <name> · /channel kick <user>
channel-owner-server = server
chat-admin-no-telemetry = Nothing received from them this run.
