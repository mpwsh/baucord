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
save-and-close = Save and close
reset-defaults = Reset to defaults
reset-confirm-body = Every setting goes back to its default, including your saved servers, nickname and picture.
reset-confirm = Reset everything

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
chat-admin-role-created = Role created (id { $id })
chat-admin-blobs-header = { $count } blob(s) stored
chat-admin-audit-header = { $count } audit entries, newest first
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
chat-user-joined-channel = { $name } joined { $channel }
chat-user-moved = { $name } moved to { $channel }
chat-you-moved = You joined { $channel }
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
audio-output-device-android = Play sound on
audio-output-hint-android = Calls and stream sound. Automatic uses a headset when one is connected, otherwise the speaker.
audio-route-auto = Automatic
audio-route-speaker = Speaker
audio-route-earpiece = Phone earpiece
audio-route-wired = Wired headset
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
menu-copy-image = Copy image

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
chat-deleted-placeholder = Message deleted
chat-message-edited = Message #{ $id } was edited: { $text }
chat-message-deleted = Message #{ $id } was deleted.
chat-rate-limited = Slow down — chat is rate limited; try again in { $secs } s.
chat-level-up = Level up! You are now level { $level } ({ $xp } xp).
chat-moved-idle = You were moved to the AFK channel after { $min } idle minutes.
chat-command-usage-channel = Usage: /channel list · /channel create <name> [password] · /channel keep <name> · /channel delete <name> · /channel kick <user>
channel-owner-server = server
chat-admin-no-telemetry = Nothing received from them this run.

# ── 1.1 roster menu, badges, moderation dialog ─────────────────────────
menu-volume = Volume
menu-mute-for-me = Mute for me
menu-unmute-for-me = Unmute for me
menu-role = Role
menu-move-to = Move to
menu-channel-kick = Send to the default channel
menu-mute = Mute
menu-mute-voice = Voice…
menu-mute-chat = Chat…
menu-unmute-voice = Unmute voice
menu-unmute-chat = Unmute chat
menu-disconnect = Disconnect…
menu-kick = Kick…
menu-ban = Ban…
role-guest = Guest
role-member = Member
role-organizer = Organizer
role-admin = Admin
tooltip-level = Level { $level }
card-xp-line = { $have } / { $need } xp to level { $next }
tooltip-telemetry-age = Telemetry { $ago }
tooltip-no-telemetry = No telemetry received from them yet.
moderation-duration = Duration
moderation-reason = Reason
moderation-reason-placeholder = Optional — shown to them and kept in the log
server-not-persistent-note = This server does not save changes. Roles, bans and invites last until it restarts.
mod-title-mute-voice = Mute { $name }'s voice?
mod-title-mute-chat = Mute { $name } in chat?
mod-title-disconnect = Disconnect { $name }?
mod-title-kick = Kick { $name }?
mod-title-ban = Ban { $name }?
mod-hint-mute-voice = They stay connected; the server drops their voice until the mute ends.
mod-hint-mute-chat = They stay connected; the server drops their messages and images until the mute ends.
mod-hint-disconnect = Their connection is closed. Rejoining takes one click.
mod-hint-kick = Their connection is closed and this device is forgotten: they need a new invite to come back.
mod-hint-ban = Their connection is closed, and their user, device and address are refused until the ban ends.
mod-confirm-mute = Mute { $name }
mod-confirm-disconnect = Disconnect { $name }
mod-confirm-kick = Kick { $name }
mod-confirm-ban = Ban { $name }
duration-10m = 10 minutes
duration-30m = 30 minutes
duration-2h = 2 hours
duration-24h = 24 hours
duration-7d = 7 days
duration-30d = 30 days
duration-forever = Until lifted

# ── 1.1 chat: edit / delete / load earlier ────────────────────────────
menu-edit-message = Edit message
menu-delete-message = Delete message
chat-editing = Editing your message — Enter saves, Esc cancels
chat-load-earlier = Load earlier messages
chat-loading = Loading…
chat-no-earlier = No earlier messages.
chat-edit-not-applied = The server didn't apply the edit — the message may be too old to change.
chat-delete-not-applied = The server didn't delete the message — it may be too old to change.
confirm-delete-message-title = Delete this message?
confirm-delete-message-body = It is removed for everyone.
confirm-delete-message-body-admin = It is removed for everyone; the audit log keeps a line.
confirm-delete-message = Delete message

# ── 1.1 channels from the UI ─────────────────────────────────────────
channel-new = New channel
channel-name = Name
channel-name-placeholder = Up to 40 characters
channel-password-placeholder = Optional
channel-limit = User limit
channel-limit-placeholder = No limit
channel-afk = AFK after (minutes)
channel-afk-placeholder = Server default
channel-keep = Keep this channel
channel-keep-hint = Otherwise it is removed after standing empty for a while.
channel-members-only = Members only
channel-members-only-hint = Guests do not see it and cannot join.
channel-create = Create channel
channel-save = Save changes
channel-not-persistent-note = This server does not save channels: it lasts until the server restarts.
channel-dialog-title-new = New channel
channel-dialog-title-edit = Edit { $name }
channel-password-unknown = This channel has a password this device does not know. Type it to keep it, or leave the field empty to remove it.
channel-error-name = Give the channel a name of up to 40 characters.
channel-error-limit = The user limit must be a whole number of 1 or more, or empty.
channel-error-afk = AFK minutes must be a whole number of 1 or more, or empty.
channel-denied-name-taken = A channel with that name already exists.
channel-denied-no-permission = You may not do that to this channel.
channel-denied-config-owned = That channel is set in the server's config file and cannot be changed here.
channel-denied-limit = You already own as many channels as this server allows.
channel-denied-no-such = That channel no longer exists.
channel-denied-invalid = The server did not accept those settings.
menu-channel-edit = Edit channel…
menu-channel-delete = Delete channel…
menu-channel-config-owned = Set in the server's config file
confirm-delete-channel-title = Delete { $name }?
confirm-delete-channel-body = Everyone in it is moved to the default channel.
confirm-delete-channel = Delete channel

menu-select-text = Select text
menu-show-formatting = Show formatting
channel-join = Join channel

# ── Settings: profile tab, general additions ──────────────────────────
tab-profile = Profile
identity-regenerate-confirm = Regenerate — new user, are you sure?
profile-lead = How you appear to others.
profile-no-name = No nickname yet
profile-nickname-hint = Renames you for everyone while connected.
profile-name-color = Name color
profile-name-style = Name style
name-style-normal = Normal
name-style-bold = Bold
name-style-italic = Italic
profile-avatar-choose = Choose image…
profile-avatar-remove = Remove
profile-avatar-error = Couldn't use that image: { $error }
profile-role-line = On { $server }: { $role } · Level { $level }
profile-role-line-no-level = On { $server }: { $role }
profile-not-connected = Not connected — your role shows here once you are.
general-lead = Language, text size and files.
text-size = Text size
text-size-hint = Applies everywhere, right away.
log-level = Log level
log-level-hint = How much baucord.log records. Debug when reporting a problem.
log-level-error = Errors only
log-level-warn = Warnings
log-level-info = Info (default)
log-level-debug = Debug
log-level-trace = Trace (everything)
text-size-small = Small
text-size-default = Default
text-size-large = Large
text-size-larger = Larger
sounds-header = Sounds
sounds-lead = What makes a sound, and when. Ordinary messages never do.
sounds-share-started = Someone in your channel starts sharing
sounds-viewer-joined = Someone starts watching your stream
sounds-viewer-joined-hint = Softer, and only you hear it.
sounds-enabled = Enable sounds
sounds-enabled-hint = The global switch: off silences every sound.
sounds-join-leave = Someone joins or leaves your channel
files-header = Files
files-note = Settings, the identity key and the avatar live in this folder.
servers-lead = Saved servers; the default is the one the connect screen starts on.
audio-lead = Devices, input gate and output level — every change applies live.

# ── Settings: startup, notifications, chat, privacy, transmission ─────
sounds-mention = A message names you
sounds-mention-hint = Your nickname as a word, or @nickname. Off by default: the general chat can be busy. Ordinary messages never make a sound.
startup-header = Startup
startup-auto-connect = Connect to the default server on launch
startup-auto-connect-hint = Needs a nickname; the connect screen is skipped.
startup-minimized = Start in the tray
startup-minimized-hint = The window stays hidden until you open it from the tray icon.
startup-check-updates = Check for a new version at launch
startup-check-updates-hint = Off, the app goes straight to the connect screen.
chat-header = Chat
chat-show-timestamps = Show the time on each message
chat-compact-rows = Compact rows
chat-image-size = Images you send
chat-image-size-hint = Larger pictures are shrunk to fit. Servers cap uploads at 10 MB.
image-size-max = Largest (10 MB)
privacy-header = Privacy
privacy-note = While connected, a server sees your nickname, your key's id, your roles and level, which channel you are in, and every 10 seconds a telemetry line: round-trip time, packet loss, the names of your audio and video devices, your app version and operating system. Admins on that server can read the last line in the roster. Nothing goes anywhere else.
audio-transmission = Transmission
audio-vad = Voice activity
audio-ptt = Push to talk
audio-vad-hint = Your voice is sent whenever it crosses the sensitivity below.
audio-ptt-hint = Hold to talk:
audio-ptt-unbound = No key bound yet — set one on the Keyboard tab.
audio-aec = Echo cancellation
audio-aec-hint = Keeps others from hearing themselves through your speakers. Automatic leaves headphones alone.
audio-aec-hint-windows = Keeps others from hearing themselves through your speakers. Automatic leaves headphones alone. On speakers, Windows may lower other apps while you talk (Sound settings → Communications).
audio-aec-hint-android = Keeps others from hearing themselves through your speaker. Automatic leaves headphones alone. On the speaker your phone treats voice as a call: the volume buttons set call volume.
audio-aec-auto = Automatic (speakers only)
audio-aec-always = Always
audio-aec-off = Off
aec-status-on = On — your device's echo canceller is running.
aec-status-headphones = Off — you're on headphones.
aec-status-off = Off.
aec-status-no-canceller = Unavailable — this device has no echo canceller.
aec-status-refused = Unavailable — the system wouldn't open call audio. Details are in baucord.log.
aec-status-unsupported = Not available on this system yet.
aec-status-idle = Takes effect when you join voice.
ptt-tooltip = Push to talk — hold

# ── Profiles on the wire (spec §7.1) ──────────────────────────────────
chat-user-renamed = { $old } is now { $new }.
profile-denied-reserved-color = Gold names are reserved for admins; pick another color.
profile-denied-low-contrast = That color doesn't read on the chat background; pick another.
profile-denied-rate-limited = One profile change every 30 seconds.
profile-denied-locked = An admin locked your profile.
profile-denied-bad-avatar = The server didn't accept that picture.
profile-avatar-refused = The picture was refused: { $why }
rename-denied-taken = That name is in use here.
rename-denied-invalid = That name is empty.
rename-denied-rate-limited = One rename every 30 seconds.
viewer-fullscreen = Full screen (F)
viewer-exit-fullscreen = Exit full screen (Esc)

# ── Chat day dividers ─────────────────────────────────────────────────
chat-day-today = Today
chat-day-yesterday = Yesterday
chat-day-label = { $weekday }, { $month } { $day }{ $suffix }
chat-day-label-year = { $weekday }, { $month } { $day }{ $suffix }, { $year }
weekday-mon = Monday
weekday-tue = Tuesday
weekday-wed = Wednesday
weekday-thu = Thursday
weekday-fri = Friday
weekday-sat = Saturday
weekday-sun = Sunday
month-1 = January
month-2 = February
month-3 = March
month-4 = April
month-5 = May
month-6 = June
month-7 = July
month-8 = August
month-9 = September
month-10 = October
month-11 = November
month-12 = December

# ── Direct messages (spec §6) ─────────────────────────────────────────
chat-dm-received = Direct message from { $name }: { $text }
dm-failed-offline = They're offline, and this server doesn't keep messages.
dm-failed-no-permission = Direct messages are for members.
dm-failed-no-such-user = No such user.
dm-failed-store = The server couldn't store the message. Try again.

messages-header = Messages
voice-header = Voice
lobby-banner = Join a voice channel to start talking
drop-to-send = Drop to send
lobby-mic-hint = Join a channel to talk
messages-collapse = Collapse messages
messages-expand = Expand messages
invite-button = Invite users
card-offline = Offline
invite-dialog-title = Invite users
invite-role = Joins as
invite-expiry = Expires
invite-uses-label = Uses
invite-create = Create invite
invite-copy = Copy code
invite-close = Close
menu-save-file = Save file…
file-save = Save
file-uploading-pct = Uploading
file-downloading-pct = Downloading
file-uploading = Uploading { $name } ({ $size })…
file-upload-failed = { $name } was not uploaded: { $why }
file-post-failed = The file was not posted: { $why }
file-too-large = { $name } is larger than { $max } MB
file-read-error = Unable to read { $name }: { $error }
file-downloading = Downloading { $name }…
notif-channel-voice = Voice connection
notif-connected-title = Connected to { $server }
notif-connected-text = Tap to return to the app.
file-pick-busy = Still reading the last file you picked. Try again in a moment.
file-saved = Saved to { $path }
file-save-error = Unable to save { $name }: { $error }
file-download-failed = { $name } is no longer on the server
dm-failed-no-such-blob = The file is not on the server
invite-copied = Invite code copied
invite-share-hint = Give the code together with the server's address and port.
invite-expiry-1h = 1 hour
invite-expiry-1d = 1 day
invite-expiry-7d = 7 days
invite-expiry-30d = 30 days
invite-expiry-note = Longer than a day needs the invite-management permission; the server shortens it otherwise.
invite-uses-1 = 1 use
invite-uses-5 = 5 uses
invite-uses-10 = 10 uses
invite-uses-unlimited-option = Unlimited
chat-empty-title = No messages yet
chat-empty-general = Say something to everyone on this server.
chat-empty-dm = Say hello to start the conversation.
dm-general = General
dm-direct-message = Direct message
dm-note-memory = Reaches them only while they are online; this server keeps nothing.
dm-back = Back to General
chat-placeholder-dm = Message
menu-message = Message…
sounds-dm = A direct message arrives
chat-dm-with-offline = { $name } is offline; the message waits on the server.
chat-command-usage-dm = Usage: /dm <name> <message>
chat-dm-unknown = Nobody here is called { $name }.
dm-close = Close conversation

# ── Admin panel (spec §8) ──
admin-title = Admin
admin-button = Admin
admin-refresh = Refresh
admin-loading = Loading…
admin-none = Nothing here.
admin-users = Users
admin-users-lead = Everyone the server knows, online or not. Actions are kept in the audit with the reason.
admin-reason = Reason
admin-reason-placeholder = Shown to them and kept in the audit
admin-disconnect = Disconnect
admin-kick = Kick
admin-ban = Ban
admin-reset-profile = Reset profile
admin-purge = Purge
admin-set-role = Set role…
admin-roles = Roles
admin-roles-lead = Permission sets. A role's standing follows what it can do.
admin-holders = holder(s)
admin-builtin = built-in
admin-edit = Edit
admin-delete = Delete
admin-role-new = New role
admin-role-editing = Edit role
admin-role-name = Name
admin-role-name-hint = 1–32 lowercase letters, digits and hyphens.
admin-role-rank-hint = Where the role stands for moderation follows from its permissions: everything a member has ranks above members, everything an organizer has above organizers.
admin-role-save = Save role
admin-restrictions = Restrictions
admin-restrictions-lead = Bans, mutes and locks in force.
admin-lift = Lift
admin-invites = Invites
admin-invites-lead = Codes that admit people, and how many times they were used.
admin-revoke = Revoke
admin-revoked = revoked
admin-channels = Channels
admin-channels-lead = Voice channels; the ones from the config file cannot be deleted here.
admin-members = member(s)
admin-config-owned = from the config file
admin-files = Files
admin-files-lead = Everything stored on the server, with who uploaded it.
admin-refs = ref(s)
admin-audit = Audit
admin-audit-lead = Who did what, newest first.
admin-audit-more = Load older
admin-server = Server
admin-server-lead = The server's counters.
admin-stat-version = Version
admin-stat-uptime = Uptime
admin-stat-online = Online
admin-stat-users = Users known
admin-stat-messages = Messages
admin-stat-blobs = Files stored
admin-stat-persistence = Persistence
admin-stat-queue = Write queue
admin-stat-dropped = Dropped writes
admin-stat-errors = Write errors
admin-yours = yours
admin-now = now
admin-storage = { $used } of { $cap } stored
admin-blobs-line = { $count } files, { $used } of { $cap }
admin-persistent-on = on (SQLite)
admin-persistent-off = off (memory only)
admin-standing-admin = admin
admin-standing-above-organizers = above organizers
admin-standing-above-members = above members
admin-standing-above-guests = above guests
admin-role-badge = Badge
admin-confirm-disconnect = Disconnect { $name }?
admin-confirm-disconnect-text = They can reconnect right away. The reason is shown to them and kept in the audit.
admin-confirm-kick = Kick { $name }?
admin-confirm-kick-text = Their device keys and roles are forgotten: they will need a new invite to come back. The reason is shown to them and kept in the audit.
admin-confirm-ban = Ban { $name }?
admin-confirm-ban-text = They are disconnected and refused from now on, by identity and address, until the ban is lifted. The reason is shown to them and kept in the audit.
admin-confirm-purge = Purge { $name }?
admin-confirm-purge-text = Erases the user entirely — account, keys, roles, and every message they ever wrote — as if they had never joined. This cannot be undone. Meant for ghosts and spam; ban instead if they might come back.
admin-confirm-reset-profile = Reset { $name }'s profile?
admin-confirm-reset-profile-text = Clears their name color, name style and picture. They can set them again afterwards.
admin-confirm-delete-role = Delete the role { $name }?
admin-confirm-delete-role-text = Whoever holds it loses it now; their other roles stay.
admin-confirm-delete-channel = Delete the channel { $name }?
admin-confirm-delete-channel-text = People in it are moved to the default channel.
admin-confirm-delete-file = Delete this file?
admin-confirm-delete-file-text = It is removed from the server; the messages that posted it keep their card, but Save will say it is gone.
admin-confirm-revoke = Revoke the invite { $code }?
admin-confirm-revoke-text = The code stops admitting anyone. People already in stay.
admin-confirm-lift = Lift this restriction?
admin-confirm-lift-text = It ends now, and the audit says who lifted it.
admin-col-when = WHEN
admin-col-who = WHO
admin-col-what = WHAT
admin-col-target = ON
admin-col-detail = DETAILS
admin-actor-server = server
admin-act-join = joined
admin-act-kick = kicked
admin-act-disconnect = disconnected
admin-act-move = moved
admin-act-roles = set roles
admin-act-purge = purged
admin-act-restrict = restricted
admin-act-lift = lifted a restriction
admin-act-message-delete = deleted a message
admin-act-invite-create = created an invite
admin-act-invite-revoke = revoked an invite
admin-act-key-link = linked a key
admin-act-key-unlink = unlinked a key
admin-act-profile-reset = reset a profile
admin-act-role-create = created a role
admin-act-role-edit = edited a role
admin-act-role-delete = deleted a role
admin-act-channel-create = created a channel
admin-act-channel-edit = edited a channel
admin-act-channel-delete = deleted a channel
admin-act-blob-delete = deleted a file
file-download-broken = The download of { $name } failed; try again
file-open = Open
file-save-again = Download again
file-gone = That file is no longer in your Downloads folder
file-open-error = Unable to open the file: { $error }
file-cancel = Cancel
admin-channel = Admin logs
admin-channel-lead = The audit, as it happens. Slash commands work here: /kick, /ban, /mute, /grant, /invite…
admin-channel-commands-only = This channel takes commands only — start with /

# ── Android host (ANDROID-SPEC.md; one window per process) ───────────
host-single-window = Not available on Android yet — this opens in its own window on desktop.
