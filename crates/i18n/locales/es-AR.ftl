# ── Vista de conexión ───────────────────────────────────────────────────
app-tagline = Tenemos discord en casa
nickname = Apodo
connect = Conectar
settings = Configuración
quit = Salir
error-nickname-empty = El apodo no puede estar vacío

# ── Aviso de dispositivo faltante ───────────────────────────────────────
device-missing-mic = el micrófono "{ $name }"
device-missing-output = la salida "{ $name }"
device-warning = { $devices } que guardaste no está conectado
device-warning-hint = Abrí Configuración para elegir otro dispositivo
list-and = y

# ── Ventana de configuración ────────────────────────────────────────────
settings-title = Configuración
tab-general = General
tab-audio = Audio
tab-keyboard = Controles
tab-credits = Créditos
tab-about = Acerca de
save-and-close = Guardar y Cerrar
reset-defaults = Restablecer

# ── Panel general ───────────────────────────────────────────────────────
language = Idioma
language-system = Idioma del sistema
language-note =
    Se aplica inmediatamente. Las ventanas que ya están abiertas se
    actualizan al reabrirlas, y el historial del chat conserva su
    idioma original.
# ── Server config (connect + settings) ──────────────────────────────────
server-password = Contraseña del servidor (opcional)
settings-server = Servidor
tab-servers = Servidores
server-name = Nombre
server-ip = IP / host
server-port = Puerto
server-default = Predeterminado
server-add = Agregar servidor
server-remove = Quitar
server-manage = Administrar servidores
server-new-name = Servidor nuevo
save = Guardar
connect-access-denied = Acceso denegado: contraseña — escribila abajo, Guardá y volvé a Conectar
server-fingerprint = Identidad del servidor (SHA256:…, de la invitación; se aprende en la primera conexión)
server-config-note =
    La contraseña es el secreto de la sala que comparten todos en tu servidor. La identidad se aprende en la primera conexión y queda fijada; pegala desde una invitación para verificar el servidor de antemano. Ambas aplican en la próxima conexión.

# ── Barra lateral ───────────────────────────────────────────────────────
users-count = Usuarios: { $count }
status-connecting = Conectando ...
status-connected = Conectado
status-disconnected = Desconectado
mute = Mutear
unmute = Desmutear
deafen = Ensordecer
undeafen = Desensordecer
share-screen = Compartir pantalla
stop-sharing = Dejar de compartir
disconnect = Desconectar
user-volume = Volumen del usuario
reset-audio = Restablecer audio
watch-stream = Ver transmisión
live = En vivo
stream-stats-quality = { $quality } · { $viewers } viendo
stream-stats-frame = { $kb }KB · { $ms }ms cod
stream-stats-degraded = { $fps }% fps · pérdida { $loss }%
latency-line = Audio: { $audio } | Server: { $network } | ↑{ $up } ↓{ $down } Mb


# ── Chat ────────────────────────────────────────────────────────────────
send = Enviar
chat-disconnected = Desconectado: { $reason }
disconnect-reason-user = El usuario se desconectó
chat-user-joined = { $name } se unió
chat-user-left = { $name } se fue
chat-share-started = Empezaste a compartir pantalla ({ $quality })
chat-share-stopped = Dejaste de compartir pantalla
chat-peer-sharing = { $name } empezó a compartir su pantalla
chat-peer-stopped-sharing = { $name } dejó de compartir su pantalla
chat-watching = Viendo la pantalla de { $name }...
chat-stopped-watching = Dejaste de ver a { $name }.
chat-mic-connected = Micrófono conectado: { $name }
chat-mic-disconnected = Micrófono desconectado: { $name }
chat-speaker-connected = Parlante conectado: { $name }
chat-speaker-disconnected = Parlante desconectado: { $name }
chat-input-lost = Se perdió el dispositivo de entrada — cambiando al predeterminado del sistema
chat-input-default-changed = Entrada predeterminada cambiada a: { $name }
chat-output-lost = Se perdió el dispositivo de salida — cambiando al predeterminado del sistema
chat-output-default-changed = Salida predeterminada cambiada a: { $name }
unknown-device = dispositivo desconocido

# ── Bandeja y visor ─────────────────────────────────────────────────────
tray-show = Mostrar
tray-quit = Salir
viewer-title = Pantalla de { $name } ({ $width }x{ $height })
stop-watching = Dejar de ver
viewer-stats = { $res } { $fps }fps { $kbps }KB/s { $mb }MB | búfer { $depth }/{ $target }ms jitter { $jitter }ms

# ── Panel de audio ──────────────────────────────────────────────────────
audio-devices-header = DISPOSITIVOS
audio-input-device = Dispositivo de entrada (Micrófono)
audio-output-device = Dispositivo de salida (Parlantes/Auriculares)
audio-input-header = ENTRADA
audio-capture-level = Nivel de captura
audio-input-gain = Ganancia de entrada
audio-input-sensitivity = Sensibilidad de entrada
audio-gate-hint = El audio debajo de la línea roja no se transmite
audio-test-mic = Probar micrófono
audio-stop-testing = Detener prueba
audio-test-hint = Escuchá tu propio micrófono (los demás no te escuchan)
audio-testing-hint = Escuchando tu micrófono... los demás no te escuchan
audio-output-header = SALIDA
audio-master-volume = Volumen principal
audio-system-default = Predeterminado del sistema
audio-device-count = { $inputs } dispositivos de entrada, { $outputs } de salida
audio-gain-value = Ganancia: { $percent }%
audio-volume-value = Volumen: { $percent }%
audio-sensitivity-line = Sensibilidad: { $name }  (umbral: { $threshold } RMS)
sensitivity-very-high = Muy alta
sensitivity-high = Alta
sensitivity-medium = Media
sensitivity-low = Baja
sensitivity-very-low = Muy baja

# ── Acciones de atajos ──────────────────────────────────────────────────
action-toggle-mute = Alternar mute
action-toggle-deafen = Alternar ensordecer
action-toggle-share = Alternar compartir pantalla
action-push-to-talk = Pulsar para hablar

# ── Panel de atajos ─────────────────────────────────────────────────────
keybinds-header = ATAJOS DE TECLADO
keybinds-hint = Hacé clic en Grabar y presioná una combinación de teclas. Esc cancela, Retroceso borra.
keybind-not-set = Sin asignar
keybind-record = Grabar
keybind-press-combo = Presioná una combinación...

# ── Diálogo de compartir pantalla ───────────────────────────────────────
monitor = Monitor
monitor-primary = (principal)
quality = Calidad
start-sharing = Empezar a compartir
error-no-monitors = No se encontraron monitores

# ── Panel de créditos ───────────────────────────────────────────────────
credits-header = CRÉDITOS
credits-super-testers = SÚPER BETA TESTERS
credits-beta-testers = BETA TESTERS

# ── Panel Acerca de ─────────────────────────────────────────────────────
about-header = ACERCA DE
about-running-from = Ejecutando desde:
about-config-file = Archivo de configuración:
about-open-config = Abrir carpeta de configuración
about-open-folder = Abrir carpeta de instalación
about-built-with = HECHO CON
tech-rust = Rust - Lenguaje de programación
tech-fltk = FLTK - Interfaz de usuario
tech-quinn = quinn-proto - Transporte QUIC
tech-ffmpeg = FFmpeg - Codificación/decodificación de video
tech-xcap = Xcap - Captura de pantalla
tech-cpal = CPAL - Entrada/salida de audio
tech-opus = Opus - Códec de audio
tech-rnnoise = RNNoise - Supresión de ruido
tech-rubato = Rubato - Remuestreo de audio

# ── Flujo de actualización ──────────────────────────────────────────────
splash-checking = Buscando actualizaciones...
update-available-title = Actualización disponible
update-available-heading = ¡Nueva versión disponible!
update-version-info =
    Tu versión: { $current }
    Última versión: { $latest }
update-skip = Omitir
update-now = Actualizar
update-downloading-title = Actualizando
update-downloading-heading = Descargando actualización v{ $version }...
update-starting-download = Iniciando descarga...
update-progress = Descargando... { $percent }%
update-ready-title = Actualización lista
update-downloaded-heading = ¡Actualización descargada!
update-ready-info = Lista para instalar. La aplicación se reiniciará automáticamente.
update-not-now = Ahora no
# Se muestra en un label de FLTK — el código escapa '&' antes de mostrarlo.
update-install-restart = Instalar y reiniciar
update-failed-title = Error de actualización
update-failed-heading = Falló la descarga
update-continue = Continuar
cancel = Cancelar

# ── Errores y compartidos ───────────────────────────────────────────────
error-invalid-address = Dirección inválida (usá ip:puerto)
error-screen-share = Compartir pantalla: { $error }
fallback-username = Usuario{ $id }
unknown = Desconocido

# ── Chat input, image attach & context menus ────────────────────────────
chat-emoji-tooltip = Emojis — o escribí códigos :nombre: como :joy:
chat-attach-tooltip = Enviá una imagen desde tu computadora
chat-clear-tooltip = Limpiar el chat (solo para vos)
chat-image-filter = Imágenes
chat-image-load-error = No se pudo cargar la imagen: { $error }
chat-image-undisplayable = { $name } envió una imagen que no se pudo mostrar
image-preview-title = Enviar imagen
image-save-filter = Imagen PNG
image-window-title = { $name } — imagen
menu-cut = Cortar
menu-copy = Copiar
menu-paste = Pegar
menu-copy-message = Copiar mensaje
menu-copy-link = Copiar enlace
menu-open-link = Abrir enlace
menu-open-image = Abrir imagen
menu-save-image = Guardar imagen…

# ── Resource stats (sidebar) ────────────────────────────────────────────
resource-line = CPU { $cpu }% · Mem { $mem } MB

# ── GIFs & media caps ───────────────────────────────────────────────────
chat-gif-too-large = El GIF supera el límite de { $mb } MB
chat-gif-too-big = Los GIF animados pueden tener como máximo { $dim } px de ancho o alto
image-save-filter-gif = Imagen GIF
chat-upload-progress = Enviando imagen… { $pct }%

# ── Agregados de la migración a Slint ───────────────────────────────────
tech-slint = Slint - Interfaz de usuario
chat-image-too-large = La imagen es demasiado grande para enviar ({ $kb } KB después de reducirla)
chat-image-save-error = No se pudo guardar la imagen: { $error }
chat-downloading-image = Descargando imagen…

# ── Canales de voz ──
channel-password-title = Contraseña del canal
channel-wrong-password = Contraseña incorrecta — probá de nuevo
channel-full = Ese canal está lleno
share-visibility = Visible para
share-channel-only = Mi canal
share-everyone = Todos
chat-channels-updated = se actualizaron los canales de voz

# Fijación de identidad del servidor (1.0)
error-identity-changed = La identidad del servidor cambió: se esperaba { $expected }, se recibió { $observed }. No se envió nada. Si el administrador rotó la clave a propósito, editá la identidad del servidor en Ajustes (o borrala) y conectá de nuevo.
chat-identity-pinned = Identidad del servidor fijada: { $fingerprint }
