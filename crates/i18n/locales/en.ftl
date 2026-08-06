# ── Connect view ────────────────────────────────────────────────────────
app-tagline = We have discord at home
nickname = Nickname
server-address = Server Address:Port
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

# ── Sidebar ─────────────────────────────────────────────────────────────
users-count = Users: { $count }
status-connecting = Connecting ...
status-connected = Connected
status-disconnected = Disconnected
latency-line = Audio: { $audio } | Network: { $network }
mute = Mute
unmute = Unmute
deafen = Deafen
share-screen = Share Screen
stop-sharing = Stop Sharing
disconnect = Disconnect
user-volume = User Volume

# ── Chat ────────────────────────────────────────────────────────────────
send = Send
chat-disconnected = Disconnected: { $reason }
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

# ── Keybind actions ─────────────────────────────────────────────────────
action-toggle-mute = Toggle Mute
action-toggle-deafen = Toggle Deafen
action-toggle-share = Toggle Screen Share
action-push-to-talk = Push to talk
