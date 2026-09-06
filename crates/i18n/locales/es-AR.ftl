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
invite-code = Código de invitación
invite-code-placeholder = p. ej. K7FQ-2MX9-ABCD — solo hace falta la primera vez
server-invite-note = Se manda una sola vez, en tu primera visita; después la clave de este dispositivo es el login.
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
server-fingerprint = Identidad del servidor (SHA256:…, de la invitación; se aprende en la primera conexión)
server-config-note =
    La identidad se aprende en la primera conexión y queda fijada; pegala desde el log de inicio del servidor para verificarlo de antemano. Aplica en la próxima conexión.

# ── Veredictos de admisión (tarjeta de conexión) ────────────────────────
connect-need-invite = Este servidor todavía no conoce este dispositivo. Escribí el código de invitación que te dieron y apretá Conectar.
connect-invite-expired = Ese código de invitación venció — pedí uno nuevo.
connect-invite-revoked = Ese código de invitación fue revocado — pedí uno nuevo.
connect-invite-exhausted = Ese código de invitación ya se usó — pedí uno nuevo.
connect-invite-unknown = Ese no es un código de invitación válido. Revisalo y probá de nuevo.
connect-key-retired = Este dispositivo fue desvinculado de tu usuario en este servidor. Pedile a un admin que lo vuelva a vincular, o regenerá tu identidad en Ajustes para empezar como usuario nuevo.
connect-name-in-use = Ese apodo ya está en uso en este servidor — elegí otro.
error-invite-code-malformed = Eso no parece un código ni un link de invitación.
error-no-identity = No se pudo cargar tu clave de identidad ({ $path }). Arreglá o borrá el archivo y reiniciá.
error-banned = Estás baneado de este servidor{ $until }{ $reason }
ban-for = por { $left }
ban-indefinite = indefinidamente
removed-disconnected = Desconectado por { $by }: { $reason }
removed-kicked = Expulsado por { $by }: { $reason } — vas a necesitar una invitación nueva para volver.
removed-banned = Baneado por { $by }{ $until }{ $reason }
removed-replaced = Iniciaste sesión desde otro dispositivo o conexión; esta se cerró.
chat-server-not-persistent = Este servidor no guarda nada entre reinicios: roles, invitaciones y baneos duran hasta que se apaga.

# ── Invitaciones (/invite en el compositor) ─────────────────────────────
chat-invite-created = Código de invitación { $code } (copiado) — { $role }, { $expiry }, { $uses }. Pasalo junto con la dirección y el puerto del servidor.
chat-invite-usage = Uso: /invite [guest|member|organizer] [24h|7d] [usos]
chat-invite-no-permission = No podés crear esa invitación acá.
chat-invite-role-not-allowed = Los códigos de invitación no pueden dar ese rol.
invite-uses = { $count } uso(s)
invite-uses-unlimited = usos ilimitados
invite-expires-in = vence en { $left }
invite-never-expires = no vence

# ── Moderación (comandos; spec §4, §8) ──────────────────────────────────
chat-commands-help = Comandos: { $commands }
chat-command-unknown = Comando desconocido /{ $command }. Probá /help.
chat-command-usage = Uso: /{ $command } nombre …
chat-command-usage-revoke-invite = Uso: /revoke-invite <código>
chat-command-usage-unlink = Uso: /unlink <hash de clave, 64 dígitos hex> (ver /users)
chat-command-usage-link = Uso: /link <nombre> <hash de clave, 64 dígitos hex>
chat-command-no-such-user = No hay nadie conectado que se llame { $name }.
chat-admin-refused = Rechazado: { $why }.
chat-admin-done = Listo.
chat-server-muted-locked = El servidor te silenció; vas a poder activar el micrófono cuando se levante.
chat-your-roles-changed = Tus roles ahora son: { $roles }
roles-none = ninguno
reason-none = (sin motivo)
chat-admin-users-header = { $count } usuario(s) registrados (● conectados):
chat-admin-restrictions-header = { $count } restricción(es):
chat-admin-invites-header = { $count } invitación(es):
restricted-voice = El servidor te silenció{ $until }{ $reason }
restricted-text = No podés escribir en el chat{ $until }{ $reason }
restricted-dm = No podés mandar mensajes directos{ $until }{ $reason }
restricted-upload = No podés subir archivos{ $until }{ $reason }
restricted-share = No podés compartir pantalla{ $until }{ $reason }
restricted-profile = No podés cambiar tu perfil{ $until }{ $reason }
restricted-server = No podés entrar{ $until }{ $reason }
lifted-voice = Te levantaron el silencio.
lifted-text = Ya podés escribir en el chat de nuevo.
lifted-dm = Ya podés mandar mensajes directos de nuevo.
lifted-upload = Ya podés subir archivos de nuevo.
lifted-share = Ya podés compartir pantalla de nuevo.
lifted-profile = Ya podés cambiar tu perfil de nuevo.
lifted-server = Ya podés entrar de nuevo.
restricted-by-role = tu rol no lo permite

# ── Identidad (Ajustes → General) ───────────────────────────────────────
identity-header = Identidad
identity-export = Exportar
identity-import = Importar
identity-regenerate = Regenerar
identity-note = La clave de este dispositivo es quién sos en todos los servidores. Exportala para ser el mismo usuario en otra máquina; Importar reemplaza la clave de este dispositivo; Regenerar crea una nueva (usuario nuevo — vas a necesitar una invitación).
identity-missing = sin clave de identidad
identity-exported = Identidad exportada.
identity-imported = Identidad importada — ahora sos esta clave. Reconectá para usarla.
identity-regenerated = Nueva identidad creada. La próxima conexión necesita una invitación.

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

# Resultados de conexión (1.0.1). Cortos, en el idioma del usuario; el
# motivo técnico queda en baucord.log.
error-lost-timeout = Se perdió la conexión — el servidor no responde. Revisá tu internet.
error-lost-reset = Se perdió la conexión — el servidor cerró la sesión después de que tu conexión se quedó muda.
error-lost-unreachable = No se puede llegar al servidor ahora — tu conexión está perdiendo paquetes.
error-lost-generic = Se perdió la conexión.
error-lost-gave-up = La reconexión automática se rindió; tocá Conectar para intentar de nuevo.
error-reconnecting = Reconectando en { $secs } s…
error-refused-full = El servidor está lleno en este momento.
error-refused-rate-limited = Demasiados intentos fallidos — esperá un minuto y probá de nuevo.
error-outdated = Esta versión de baucord no coincide con el servidor. Actualizá baucord y probá de nuevo.
error-local = No se pudo iniciar la conexión: { $detail }
error-lost-restart = El servidor se está reiniciando.

# ── Historial, progreso, canales (spec §5, §6, §9) ───────────────────────
chat-history-loaded = { $count } mensaje(s) anteriores{ $more ->
    [yes] ; /history carga más
   *[no] .
}
chat-history-unavailable = El historial no está disponible ahora; probá de nuevo en { $secs } s.
chat-edited-mark = (editado)
chat-deleted-placeholder = Mensaje borrado
chat-message-edited = El mensaje #{ $id } fue editado: { $text }
chat-message-deleted = El mensaje #{ $id } fue borrado.
chat-rate-limited = Más despacio: el chat tiene límite; probá de nuevo en { $secs } s.
chat-level-up = ¡Subiste de nivel! Ahora sos nivel { $level } ({ $xp } xp).
chat-moved-idle = Te movieron al canal AFK después de { $min } minutos sin actividad.
chat-command-usage-channel = Uso: /channel list · /channel create <nombre> [contraseña] · /channel keep <nombre> · /channel delete <nombre> · /channel kick <usuario>
channel-owner-server = servidor
chat-admin-no-telemetry = No llegó telemetría de esa persona en esta sesión.

# ── 1.1 menú del roster, insignias, diálogo de moderación ───────────────
menu-volume = Volumen…
menu-mute-for-me = Silenciar para mí
menu-unmute-for-me = Dejar de silenciar para mí
menu-role = Rol
menu-move-to = Mover a
menu-channel-kick = Mandar al canal principal
menu-mute = Silenciar
menu-mute-voice = Voz…
menu-mute-chat = Chat…
menu-unmute-voice = Reactivar la voz
menu-unmute-chat = Reactivar el chat
menu-disconnect = Desconectar…
menu-kick = Expulsar…
menu-ban = Banear…
role-guest = Invitado
role-member = Miembro
role-organizer = Organizador
role-admin = Admin
tooltip-level = Nivel { $level }
tooltip-telemetry-age = Telemetría { $ago }
tooltip-no-telemetry = Todavía no llegó telemetría de esta persona.
moderation-duration = Duración
moderation-reason = Motivo
moderation-reason-placeholder = Opcional — se le muestra y queda en el registro
server-not-persistent-note = Este servidor no guarda cambios. Los roles, baneos e invitaciones duran hasta que se reinicie.
mod-title-mute-voice = ¿Silenciar la voz de { $name }?
mod-title-mute-chat = ¿Silenciar a { $name } en el chat?
mod-title-disconnect = ¿Desconectar a { $name }?
mod-title-kick = ¿Expulsar a { $name }?
mod-title-ban = ¿Banear a { $name }?
mod-hint-mute-voice = Sigue conectado; el servidor descarta su voz hasta que termine el silencio.
mod-hint-mute-chat = Sigue conectado; el servidor descarta sus mensajes e imágenes hasta que termine el silencio.
mod-hint-disconnect = Se cierra su conexión. Volver a entrar es un clic.
mod-hint-kick = Se cierra su conexión y se olvida este dispositivo: necesita una invitación nueva para volver.
mod-hint-ban = Se cierra su conexión, y su usuario, dispositivo y dirección quedan rechazados hasta que termine el baneo.
mod-confirm-mute = Silenciar a { $name }
mod-confirm-disconnect = Desconectar a { $name }
mod-confirm-kick = Expulsar a { $name }
mod-confirm-ban = Banear a { $name }
duration-10m = 10 minutos
duration-30m = 30 minutos
duration-2h = 2 horas
duration-24h = 24 horas
duration-7d = 7 días
duration-30d = 30 días
duration-forever = Hasta que se levante

# ── 1.1 chat: editar / borrar / cargar anteriores ──────────────────────
menu-edit-message = Editar mensaje
menu-delete-message = Borrar mensaje
chat-editing = Editando tu mensaje — Enter guarda, Esc cancela
chat-load-earlier = Cargar mensajes anteriores
chat-loading = Cargando…
chat-no-earlier = No hay mensajes anteriores.
chat-edit-not-applied = El servidor no aplicó la edición — puede que el mensaje sea muy viejo para cambiarlo.
chat-delete-not-applied = El servidor no borró el mensaje — puede que sea muy viejo para cambiarlo.
confirm-delete-message-title = ¿Borrar este mensaje?
confirm-delete-message-body = Se elimina para todos.
confirm-delete-message-body-admin = Se elimina para todos; el registro de auditoría guarda una línea.
confirm-delete-message = Borrar mensaje

# ── 1.1 canales desde la interfaz ─────────────────────────────────────
channel-new = Canal nuevo
channel-name = Nombre
channel-name-placeholder = Hasta 40 caracteres
channel-password-placeholder = Opcional
channel-limit = Límite de usuarios
channel-limit-placeholder = Sin límite
channel-afk = AFK después de (minutos)
channel-afk-placeholder = Valor del servidor
channel-keep = Conservar este canal
channel-keep-hint = Si no, se elimina después de estar vacío un rato.
channel-members-only = Solo miembros
channel-members-only-hint = Los invitados no lo ven ni pueden entrar.
channel-create = Crear canal
channel-save = Guardar cambios
channel-not-persistent-note = Este servidor no guarda canales: dura hasta que el servidor se reinicie.
channel-dialog-title-new = Canal nuevo
channel-dialog-title-edit = Editar { $name }
channel-password-unknown = Este canal tiene una contraseña que este dispositivo no conoce. Escribila para conservarla, o dejá el campo vacío para quitarla.
channel-error-name = Dale al canal un nombre de hasta 40 caracteres.
channel-error-limit = El límite de usuarios tiene que ser un número entero de 1 o más, o vacío.
channel-error-afk = Los minutos de AFK tienen que ser un número entero de 1 o más, o vacío.
channel-denied-name-taken = Ya existe un canal con ese nombre.
channel-denied-no-permission = No podés hacer eso con este canal.
channel-denied-config-owned = Ese canal está definido en el archivo de configuración del servidor y no se puede cambiar acá.
channel-denied-limit = Ya tenés tantos canales como permite este servidor.
channel-denied-no-such = Ese canal ya no existe.
channel-denied-invalid = El servidor no aceptó esa configuración.
menu-channel-edit = Editar canal…
menu-channel-delete = Borrar canal…
menu-channel-config-owned = Definido en la configuración del servidor
confirm-delete-channel-title = ¿Borrar { $name }?
confirm-delete-channel-body = Todos los que están adentro pasan al canal principal.
confirm-delete-channel = Borrar canal

menu-select-text = Seleccionar texto
menu-show-formatting = Mostrar formato
channel-join = Entrar al canal

# ── Configuración: pestaña de perfil, adiciones a general ──────────────
tab-profile = Perfil
identity-regenerate-confirm = Regenerar — usuario nuevo, ¿seguro?
profile-lead = Cómo te ven los demás.
profile-no-name = Todavía sin apodo
profile-nickname-hint = Te renombra para todos mientras estés conectado.
profile-name-color = Color del nombre
profile-name-style = Estilo del nombre
name-style-normal = Normal
name-style-bold = Negrita
name-style-italic = Cursiva
profile-avatar-choose = Elegir imagen…
profile-avatar-remove = Quitar
profile-avatar-error = No se pudo usar esa imagen: { $error }
profile-role-line = En { $server }: { $role } · Nivel { $level }
profile-role-line-no-level = En { $server }: { $role }
profile-not-connected = Sin conexión — tu rol aparece acá cuando te conectes.
general-lead = Idioma, tamaño del texto, sonidos y archivos.
text-size = Tamaño del texto
text-size-hint = Se aplica en todos lados, al instante.
text-size-small = Chico
text-size-default = Normal
text-size-large = Grande
text-size-larger = Más grande
sounds-header = Sonidos
sounds-enabled = Reproducir sonidos
sounds-enabled-hint = Apagado, silencia todo lo de abajo.
sounds-join-leave = Alguien entra o sale de tu canal
files-header = Archivos
files-note = La configuración, la clave de identidad y el avatar viven en esta carpeta.
servers-lead = Servidores guardados; el predeterminado es con el que arranca la pantalla de conexión.
audio-lead = Dispositivos, puerta de entrada y nivel de salida — cada cambio se aplica en vivo.

# ── Configuración: inicio, notificaciones, chat, privacidad, transmisión ─
sounds-mention = Un mensaje te nombra
sounds-mention-hint = Tu apodo como palabra, o @apodo. Apagado por defecto: el chat general puede ser movido. Los mensajes comunes nunca suenan.
startup-header = Inicio
startup-auto-connect = Conectar al servidor predeterminado al abrir
startup-auto-connect-hint = Necesita un apodo; se salta la pantalla de conexión.
startup-minimized = Iniciar en la bandeja
startup-minimized-hint = La ventana queda oculta hasta que la abras desde el ícono de la bandeja.
startup-check-updates = Buscar una versión nueva al abrir
startup-check-updates-hint = Apagado, la app va directo a la pantalla de conexión.
chat-header = Chat
chat-show-timestamps = Mostrar la hora en cada mensaje
chat-compact-rows = Filas compactas
chat-image-size = Imágenes que enviás
chat-image-size-hint = Las imágenes más grandes se achican para entrar. Los servidores limitan las subidas a 10 MB.
image-size-max = Máximo (10 MB)
privacy-header = Privacidad
privacy-note = Mientras estás conectado, un servidor ve tu apodo, el id de tu clave, tus roles y nivel, en qué canal estás, y cada 10 segundos una línea de telemetría: latencia ida y vuelta, pérdida de paquetes, los nombres de tus dispositivos de audio y video, tu versión de la app y sistema operativo. Los admins de ese servidor pueden leer la última línea en el roster. Nada va a ningún otro lado.
audio-transmission = Transmisión
audio-vad = Detección de voz
audio-ptt = Pulsar para hablar
audio-vad-hint = Tu voz se envía cada vez que supera la sensibilidad de abajo.
audio-ptt-hint = Mantené apretada para hablar:
audio-ptt-unbound = Todavía no hay tecla — asigná una en la pestaña Teclado.
ptt-tooltip = Pulsar para hablar — mantené

# ── Perfiles por la red (spec §7.1) ───────────────────────────────────
chat-user-renamed = { $old } ahora es { $new }.
profile-denied-reserved-color = Los nombres dorados están reservados para admins; elegí otro color.
profile-denied-low-contrast = Ese color no se lee sobre el fondo del chat; elegí otro.
profile-denied-rate-limited = Un cambio de perfil cada 30 segundos.
profile-denied-locked = Un admin bloqueó tu perfil.
profile-denied-bad-avatar = El servidor no aceptó esa imagen.
profile-avatar-refused = La imagen fue rechazada: { $why }
rename-denied-taken = Ese nombre está en uso acá.
rename-denied-invalid = Ese nombre está vacío.
rename-denied-rate-limited = Un cambio de nombre cada 30 segundos.
viewer-fullscreen = Pantalla completa (F)
viewer-exit-fullscreen = Salir de pantalla completa (Esc)

# ── Separadores de día en el chat ─────────────────────────────────────
chat-day-today = Hoy
chat-day-yesterday = Ayer
chat-day-label = { $weekday }, { $day } de { $month }
chat-day-label-year = { $weekday }, { $day } de { $month } de { $year }
weekday-mon = lunes
weekday-tue = martes
weekday-wed = miércoles
weekday-thu = jueves
weekday-fri = viernes
weekday-sat = sábado
weekday-sun = domingo
month-1 = enero
month-2 = febrero
month-3 = marzo
month-4 = abril
month-5 = mayo
month-6 = junio
month-7 = julio
month-8 = agosto
month-9 = septiembre
month-10 = octubre
month-11 = noviembre
month-12 = diciembre

# ── Mensajes directos (spec §6) ───────────────────────────────────────
chat-dm-received = Mensaje directo de { $name }: { $text }
dm-failed-offline = No está conectado, y este servidor no guarda mensajes.
dm-failed-no-permission = Los mensajes directos son para miembros.
dm-failed-no-such-user = No existe ese usuario.
dm-failed-store = El servidor no pudo guardar el mensaje. Probá de nuevo.

messages-header = Mensajes
dm-general = General
dm-direct-message = Mensaje directo
dm-note-memory = Le llega solo mientras esté conectado; este servidor no guarda nada.
dm-back = Volver a General
chat-placeholder-dm = Mensaje para
menu-message = Mensaje…
sounds-dm = Llega un mensaje directo
chat-dm-with-offline = { $name } no está conectado; el mensaje espera en el servidor.
chat-command-usage-dm = Uso: /dm <nombre> <mensaje>
chat-dm-unknown = Acá nadie se llama { $name }.
chat-dm-no-images = Las imágenes en mensajes directos llegan con la actualización de archivos.
dm-close = Cerrar conversación
