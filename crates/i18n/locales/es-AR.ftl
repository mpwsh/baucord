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
save-and-close = Guardar y cerrar
# Settings' footer: leave without keeping what was changed since it opened.
settings-close = Cerrar
reset-defaults = Restablecer
reset-confirm-body = Todas las opciones vuelven a su valor predeterminado, incluidos tus servidores guardados, tu apodo y tu foto.
reset-confirm = Restablecer todo

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
banner-not-persistent = Este servidor no guarda nada entre reinicios: roles, invitaciones y baneos duran hasta que se apaga.

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
activity-command-done = { $command }: listo
chat-server-muted-locked = El servidor te silenció; vas a poder activar el micrófono cuando se levante.
chat-your-roles-changed = Tus roles ahora son: { $roles }
roles-none = ninguno
reason-none = (sin motivo)
chat-admin-users-header = { $count } usuario(s) registrados:
chat-admin-list-more = … y { $count } más. El panel de administración los muestra todos.
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
identity-imported = Identidad importada. Reconectá para usarla.
identity-regenerated = Nueva identidad creada. La próxima conexión necesita una invitación.

# ── Barra lateral ───────────────────────────────────────────────────────
users-count = Usuarios: { $count }
status-connected = Conectado
status-disconnected = Desconectado
mute = Mutear
unmute = Desmutear
deafen = Ensordecer
undeafen = Desensordecer
share-screen = Compartir pantalla
stop-sharing = Dejar de compartir
disconnect = Desconectar
reset-audio = Restablecer audio
watch-stream = Ver transmisión
live = En vivo
stream-stats-quality = { $quality } · { $viewers } viendo
stream-stats-frame = { $kb }KB · { $ms }ms cod
stream-stats-degraded = { $fps }% fps · pérdida { $loss }%
latency-line = Audio: { $audio } | Server: { $network }
# The transfer rates' unit, after the download rate (the arrows are drawn).
latency-unit = Mb

# ── Chat ────────────────────────────────────────────────────────────────
send = Enviar
chat-disconnected = Desconectado: { $reason }
disconnect-reason-user = El usuario se desconectó
chat-user-joined = { $name } se unió
chat-user-joined-channel = { $name } se unió a { $channel }
chat-user-moved = { $name } pasó a { $channel }
chat-you-moved = Te uniste a { $channel }
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
tray-tip-muted = baucord — muteado
tray-tip-deafened = baucord — ensordecido
# In the tray's menu, one item per person in our voice room: muting is for you alone.
tray-mute-person = Silenciar a { $name }
tray-unmute-person = Dejar de silenciar a { $name }
viewer-title = Pantalla de { $name } ({ $width }x{ $height })
stop-watching = Dejar de ver
viewer-stats = { $res } { $fps }fps { $kbps }KB/s { $mb }MB | búfer { $depth }/{ $target }ms jitter { $jitter }ms

# ── Panel de audio ──────────────────────────────────────────────────────
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
audio-testing-hint-delayed = Decí algo: lo vas a escuchar medio segundo después. Los demás no te escuchan
audio-output-header = SALIDA
audio-master-volume = Volumen principal
audio-system-default = Predeterminado del sistema
audio-output-device-android = Reproducir el sonido por
audio-output-hint-android = Llamadas y sonido de las transmisiones. En automático usa los auriculares si hay unos conectados; si no, el parlante.
audio-route-auto = Automático
audio-route-speaker = Parlante
audio-route-earpiece = Auricular del teléfono
audio-route-wired = Auriculares con cable
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
keybinds-hint = Hacé clic en Grabar y presioná una combinación de teclas. Esc cancela, Retroceso borra.
keybinds-access = macOS deja que baucord vea tus atajos mientras estás en otra app solo con el permiso de Accesibilidad. Hasta tenerlo, una tecla asignada funciona igual, pero macOS se la saca a la app en la que estás: ahí no escribe nada.
keybinds-access-ask = Permitir en Ajustes del Sistema…
keybind-not-set = Sin asignar
keybind-press-combo = Presioná una combinación...

# ── Diálogo de compartir pantalla ───────────────────────────────────────
monitor = Monitor
monitor-primary = (principal)
quality = Calidad
start-sharing = Empezar a compartir
error-no-monitors = No se encontraron monitores

# ── Panel de créditos ───────────────────────────────────────────────────
credits-super-testers = SÚPER BETA TESTERS
credits-beta-testers = BETA TESTERS

# ── Panel Acerca de ─────────────────────────────────────────────────────
about-running-from = Ejecutando desde:
about-config-file = Archivo de configuración:
about-open-config = Abrir carpeta de configuración
about-built-with = HECHO CON
tech-rust = Rust - Lenguaje de programación
tech-quinn = quinn-proto - Transporte QUIC
tech-ffmpeg = FFmpeg - Codificación/decodificación de video
tech-xcap = Xcap - Captura de pantalla
tech-cpal = CPAL - Entrada/salida de audio
tech-opus = Opus - Códec de audio
tech-rnnoise = RNNoise - Supresión de ruido
tech-rubato = Rubato - Remuestreo de audio

# ── Flujo de actualización ──────────────────────────────────────────────
splash-checking = Buscando actualizaciones...
update-available-heading = ¡Nueva versión disponible!
update-version-info =
    Tu versión: { $current }
    Última versión: { $latest }
update-skip = Omitir
update-now = Actualizar
update-downloading-heading = Descargando actualización v{ $version }...
update-progress = Descargando... { $percent }%
update-downloaded-heading = ¡Actualización descargada!
update-ready-info = Lista para instalar. La aplicación se reiniciará automáticamente.
update-not-now = Ahora no
# Se muestra en un label de FLTK — el código escapa '&' antes de mostrarlo.
update-install-restart = Instalar y reiniciar
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
chat-image-load-error = No se pudo cargar la imagen: { $error }
chat-image-undisplayable = { $name } envió una imagen que no se pudo mostrar
image-preview-title = Enviar imagen
image-save-filter = Imagen PNG
menu-cut = Cortar
menu-copy = Copiar
menu-paste = Pegar
menu-copy-message = Copiar mensaje
menu-open-image = Abrir imagen
menu-save-image = Guardar imagen…
menu-copy-image = Copiar imagen

# ── Resource stats (sidebar) ────────────────────────────────────────────
resource-line = CPU { $cpu }% · Mem { $mem } MB

# ── GIFs & media caps ───────────────────────────────────────────────────
chat-gif-too-large = El GIF supera el límite de { $mb } MB
chat-gif-too-big = Los GIF animados pueden tener como máximo { $dim } px de ancho o alto
image-save-filter-gif = Imagen GIF
image-save-filter-jpeg = Imagen JPEG

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
chat-rate-limited = Más despacio: el chat tiene límite; probá de nuevo en { $secs } s.
chat-level-up = ¡Subiste de nivel! Ahora sos nivel { $level } ({ $xp } xp).
chat-moved-idle = Te movieron al canal AFK después de { $min } minutos sin actividad.
chat-command-usage-channel = Uso: /channel list · /channel create <nombre> [contraseña] · /channel keep <nombre> · /channel delete <nombre> · /channel kick <usuario>
channel-owner-server = servidor
chat-admin-no-telemetry = No llegó telemetría de esa persona en esta sesión.

# ── 1.1 menú del roster, insignias, diálogo de moderación ───────────────
menu-volume = Volumen
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
# The admin panel's Actions menu (Users tab): each asks first.
menu-reset-profile = Restablecer perfil…
menu-purge = Purgar…
role-guest = Invitado
role-member = Miembro
role-organizer = Organizador
role-admin = Admin
tooltip-level = Nivel { $level }
card-xp-line = { $have } / { $need } xp para el nivel { $next }
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
chat-new-messages = { $count ->
    [one] 1 mensaje nuevo
   *[other] { $count } mensajes nuevos
}
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
general-lead = Idioma, tamaño del texto y archivos.
text-size = Tamaño del texto
text-size-hint = Se aplica en todos lados, al instante.
log-level = Nivel de registro
log-level-hint = Cuánto guarda baucord.log. Debug cuando reportás un problema.
log-level-error = Solo errores
log-level-warn = Advertencias
log-level-info = Info (predeterminado)
log-level-debug = Debug
log-level-trace = Trace (todo)
tab-debug = Depuración
debug-lead = Lo que la app viene registrando, y formas de compartirlo cuando algo falla: copiarlo o mandárselo al admin del servidor.
debug-log-header = Registro
debug-send-log = Enviar registro al servidor
debug-copy-log = Copiar registro
debug-clear-log = Borrar registro
debug-send-hint = Copiar pone el final del registro en el portapapeles. Enviar le manda hasta 2 MB al servidor al que estás conectado, una vez cada 10 minutos; su admin lo encuentra en la carpeta logs del servidor.
debug-tail-label = Últimas 100 líneas
debug-tail-empty = El registro está vacío.
debug-no-log-file = Esta compilación escribe el registro en la terminal, no en un archivo.
debug-log-file = Archivo de registro: { $path } ({ $size })
debug-sent = Registro enviado a { $server } ({ $size }).
debug-send-wait = Ya lo mandaste: el servidor acepta uno cada 10 minutos. Probá de nuevo en { $minutes } min.
debug-send-offline = Primero conectate a un servidor.
debug-send-empty = No hay nada para mandar: el registro está vacío.
debug-read-failed = No se pudo leer el registro: { $error }
debug-cleared = Registro borrado.
debug-clear-failed = No se pudo borrar el registro: { $error }
debug-copied = Se copió el final del registro ({ $size }).
debug-copy-empty = No hay nada para copiar: el registro está vacío.
debug-copy-failed = No se pudo copiar el registro: { $error }
text-size-small = Chico
text-size-default = Normal
text-size-large = Grande
text-size-larger = Más grande
sounds-header = Sonidos
sounds-lead = Qué eventos hacen sonar algo.
sounds-share-started = Alguien en tu canal empieza a compartir
sounds-viewer-joined = Alguien empieza a ver tu transmisión
sounds-viewer-joined-hint = Más suave, y solo lo escuchás vos.
sounds-mute-toggle = Te silenciás o ensordecés
sounds-mute-toggle-hint = Una nota suave: más grave al silenciarte, más aguda al volver.
sounds-enabled = Activar sonidos
sounds-enabled-hint = Apagado, silencia todos los sonidos.
sounds-join-leave = Alguien entra o sale de tu canal
files-header = Archivos
files-note = La configuración, la clave de identidad y el avatar viven en esta carpeta.
servers-lead = Servidores guardados; el predeterminado es con el que arranca la pantalla de conexión.
audio-lead = Los cambios se aplican al instante.

# ── Configuración: inicio, notificaciones, chat, privacidad, transmisión ─
sounds-mention = Un mensaje te nombra
sounds-mention-hint = Tu apodo como palabra completa, o @apodo.
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
privacy-note = Mientras estás conectado, un servidor ve tu apodo, el id de tu clave, tus roles y nivel, en qué canal estás, y cada 10 segundos una línea de telemetría: latencia ida y vuelta, pérdida de paquetes, los nombres de tus dispositivos de audio y video, tu versión de la app y sistema operativo. Los admins de ese servidor pueden leer la última línea en el roster.
audio-transmission = Transmisión
audio-vad = Detección de voz
audio-ptt = Pulsar para hablar
audio-vad-hint = Tu voz se envía cada vez que supera la sensibilidad de abajo.
audio-ptt-hint = Mantené apretada para hablar:
audio-ptt-unbound = Todavía no hay tecla — asigná una en la pestaña Teclado.
audio-aec = Cancelación de eco
audio-aec-hint = Evita que los demás se escuchen a sí mismos por tus parlantes. En automático no toca los auriculares.
audio-aec-hint-windows = Evita que los demás se escuchen a sí mismos por tus parlantes. En automático no toca los auriculares. Con parlantes, Windows puede bajar el volumen de otras apps mientras hablás (Configuración de sonido → Comunicaciones).
audio-aec-hint-android = Evita que los demás se escuchen a sí mismos por el parlante. En automático no toca los auriculares. Con el parlante, el teléfono trata la voz como una llamada: los botones de volumen ajustan el volumen de llamada.
audio-aec-hint-ios = Evita que los demás se escuchen a sí mismos por el parlante. En automático no toca los auriculares. Con el parlante, el iPhone trata la voz como una llamada: los botones de volumen ajustan el volumen de llamada y las otras apps suenan un poco más bajo.
audio-aec-auto = Automática (solo parlantes)
audio-aec-always = Siempre
audio-aec-off = Desactivada
aec-status-on = Activa — está funcionando la cancelación de eco de tu dispositivo.
aec-status-headphones = Desactivada — estás con auriculares.
aec-status-off = Desactivada.
aec-status-no-canceller = No disponible — este dispositivo no tiene cancelación de eco.
aec-status-canceller-off = No disponible — la cancelación de eco no se pudo activar. Los detalles están en baucord.log.
aec-status-refused = No disponible — el sistema no abrió el audio de llamada. Los detalles están en baucord.log.
aec-status-unsupported = Todavía no está disponible en este sistema.
aec-status-idle = Se aplica cuando entrás a un canal de voz.
aec-lowering = Windows baja el volumen de tus otras apps mientras baucord está conectado, y no dejó que baucord lo desactive. En la configuración de sonido de Windows, pestaña Comunicaciones, elegí "No hacer nada".
aec-lowering-fix = Abrir la configuración de sonido de Windows
aec-lowering-told = Windows baja el volumen de tus otras apps mientras baucord está conectado. En Configuración → Audio está cómo evitarlo.
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
viewer-fill = Ampliar hasta llenar (doble toque)
viewer-fit = Ajustar a la pantalla (doble toque)
viewer-rotate = Girar a horizontal
viewer-unrotate = Volver a vertical
viewer-mic-mute = Mutear tu micrófono
viewer-mic-unmute = Desmutear tu micrófono
viewer-stream-mute = Mutear la transmisión
viewer-stream-unmute = Desmutear la transmisión

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
dm-failed-offline = No está conectado, y este servidor no guarda mensajes.
dm-failed-no-permission = Los mensajes directos son para miembros.
dm-failed-no-such-user = No existe ese usuario.
dm-failed-store = El servidor no pudo guardar el mensaje. Probá de nuevo.

messages-header = Mensajes
voice-header = Voz
lobby-banner = Unite a un canal de voz para empezar a hablar
drop-to-send = Soltá para enviar
lobby-mic-hint = Unite a un canal para hablar
messages-collapse = Contraer mensajes
messages-expand = Expandir mensajes
invite-button = Invitar usuarios
card-offline = Desconectado
invite-dialog-title = Invitar usuarios
invite-role = Entra como
invite-expiry = Vence
invite-uses-label = Usos
invite-create = Crear invitación
invite-copy = Copiar código
invite-close = Cerrar
menu-save-file = Guardar archivo…
file-save = Guardar
file-uploading-pct = Subiendo
file-downloading-pct = Descargando
file-upload-failed = { $name } no se subió: { $why }
file-too-large = { $name } pesa más de { $max } MB
file-read-error = No se pudo leer { $name }: { $error }
notif-channel-voice = Conexión de voz
notif-connected-title = Conectado a { $server }
notif-connected-text = Tocá para volver a la app.
notif-channel-messages = Mensajes
notif-channel-activity = Actividad de voz
notif-dm-file = Mandó un archivo: { $name }
notif-first-join-title = { $name } entró a { $channel }
notif-first-join-text = La primera persona en voz en { $server }.
notifs-header = Notificaciones
notifs-lead = Mientras baucord está en segundo plano y sigue conectado. Los modos silencio y No molestar del teléfono valen acá, y también para los sonidos de mensajes de arriba.
notifs-messages = Mensajes directos
notifs-messages-hint = El mensaje y quién lo mandó; tocá para abrir la conversación.
notifs-first-join = Alguien empieza una llamada
notifs-first-join-hint = La primera persona que entra a un canal de voz cuando están todos vacíos. Como mucho una vez cada 10 minutos.
file-pick-busy = Todavía se está leyendo el último archivo que elegiste. Probá de nuevo en un momento.
file-saved = Guardado en { $path }
file-save-error = No se pudo guardar { $name }: { $error }
file-download-failed = { $name } ya no está en el servidor
dm-failed-no-such-blob = El archivo no está en el servidor
invite-copied = Código de invitación copiado
invite-share-hint = Pasá el código junto con la dirección y el puerto del servidor.
invite-expiry-1h = 1 hora
invite-expiry-1d = 1 día
invite-expiry-7d = 7 días
invite-expiry-30d = 30 días
invite-expiry-note = Más de un día requiere el permiso de gestión de invitaciones; si no, el servidor lo acorta.
invite-uses-1 = 1 uso
invite-uses-5 = 5 usos
invite-uses-10 = 10 usos
invite-uses-unlimited-option = Ilimitados
chat-empty-title = Todavía no hay mensajes
chat-empty-general = Escribí algo para todos en este servidor.
chat-empty-dm = Saludá para empezar la conversación.
dm-general = General
dm-direct-message = Mensaje directo
dm-note-memory = Le llega solo mientras esté conectado; este servidor no guarda nada.
dm-back = Volver a General
chat-placeholder-dm = Mensaje para
menu-message = Mensaje…
sounds-dm = Llega un mensaje directo
chat-command-usage-dm = Uso: /dm <nombre> <mensaje>
chat-dm-unknown = Acá nadie se llama { $name }.

# ── Admin panel (spec §8) ──
admin-title = Administración
admin-button = Admin
admin-refresh = Actualizar
admin-loading = Cargando…
admin-none = Nada por aquí.
admin-users = Usuarios
admin-users-lead = Todos los que el servidor conoce, conectados o no. Las acciones quedan en la auditoría con el motivo.
admin-reason = Motivo
admin-reason-placeholder = Se les muestra y queda en la auditoría
admin-disconnect = Desconectar
admin-kick = Expulsar
admin-ban = Banear
admin-reset-profile = Restablecer perfil
admin-purge = Purgar
admin-set-role = Asignar rol…
# A person's row in the admin panel: the button that opens their actions.
admin-actions = Acciones
admin-roles = Roles
admin-roles-lead = Conjuntos de permisos. La posición de un rol sigue a lo que puede hacer.
admin-holders = titular(es)
admin-builtin = integrado
admin-edit = Editar
admin-delete = Borrar
admin-role-new = Rol nuevo
admin-role-editing = Editar rol
admin-role-name = Nombre
admin-role-name-hint = 1–32 letras minúsculas, dígitos y guiones.
admin-role-rank-hint = La posición del rol para moderar sigue a sus permisos: todo lo que tiene un miembro queda por encima de los miembros, todo lo de un organizador por encima de los organizadores.
admin-role-save = Guardar rol
admin-restrictions = Restricciones
admin-restrictions-lead = Baneos, silencios y bloqueos vigentes.
admin-lift = Levantar
admin-invites = Invitaciones
admin-invites-lead = Códigos que dan entrada, y cuántas veces se usaron.
admin-revoke = Revocar
admin-revoked = revocada
admin-channels = Canales
admin-channels-lead = Canales de voz; los del archivo de configuración no se borran acá.
admin-members = miembro(s)
admin-config-owned = del archivo de configuración
admin-files = Archivos
admin-files-lead = Todo lo guardado en el servidor, con quién lo subió.
admin-refs = ref(s)
admin-audit = Auditoría
admin-audit-lead = Quién hizo qué, lo más nuevo primero.
admin-audit-more = Cargar anteriores
admin-server = Servidor
admin-server-lead = Los contadores del servidor.
admin-stat-version = Versión
admin-stat-uptime = En marcha
admin-stat-online = Conectados
admin-stat-users = Usuarios conocidos
admin-stat-messages = Mensajes
admin-stat-blobs = Archivos guardados
admin-stat-persistence = Persistencia
admin-stat-queue = Cola de escritura
admin-stat-dropped = Escrituras perdidas
admin-stat-errors = Errores de escritura
admin-yours = tuyo
admin-now = ahora
admin-storage = { $used } de { $cap } usados
admin-blobs-line = { $count } archivos, { $used } de { $cap }
admin-persistent-on = activa (SQLite)
admin-persistent-off = apagada (solo memoria)
admin-standing-admin = admin
admin-standing-above-organizers = por encima de organizadores
admin-standing-above-members = por encima de miembros
admin-standing-above-guests = por encima de invitados
admin-role-badge = Insignia
admin-confirm-disconnect = ¿Desconectar a { $name }?
admin-confirm-disconnect-text = Puede volver a conectarse enseguida. El motivo se le muestra y queda en la auditoría.
admin-confirm-kick = ¿Expulsar a { $name }?
admin-confirm-kick-text = Se olvidan sus claves y roles: necesitará una invitación nueva para volver. El motivo se le muestra y queda en la auditoría.
admin-confirm-ban = ¿Banear a { $name }?
admin-confirm-ban-text = Se desconecta y se le niega la entrada desde ahora, por identidad y dirección, hasta que se levante el baneo. El motivo se le muestra y queda en la auditoría.
admin-confirm-purge = ¿Purgar a { $name }?
admin-confirm-purge-text = Borra al usuario por completo — cuenta, claves, roles y cada mensaje que escribió — como si nunca hubiera entrado. No se puede deshacer. Es para fantasmas y spam; si puede volver, mejor banear.
admin-confirm-reset-profile = ¿Restablecer el perfil de { $name }?
admin-confirm-reset-profile-text = Borra el color y estilo del nombre y la foto. Puede volver a ponerlos después.
admin-confirm-delete-role = ¿Borrar el rol { $name }?
admin-confirm-delete-role-text = Quien lo tenga lo pierde ahora; sus otros roles quedan.
admin-confirm-delete-channel = ¿Borrar el canal { $name }?
admin-confirm-delete-channel-text = Quienes estén adentro pasan al canal por defecto.
admin-confirm-delete-file = ¿Borrar este archivo?
admin-confirm-delete-file-text = Se quita del servidor; los mensajes que lo publicaron conservan la tarjeta, pero Guardar dirá que ya no está.
admin-confirm-revoke = ¿Revocar la invitación { $code }?
admin-confirm-revoke-text = El código deja de admitir a nadie. Quienes ya entraron se quedan.
admin-confirm-lift = ¿Levantar esta restricción?
admin-confirm-lift-text = Termina ahora, y la auditoría registra quién la levantó.
admin-col-when = CUÁNDO
admin-col-who = QUIÉN
admin-col-what = QUÉ
admin-col-target = A
admin-col-detail = DETALLES
admin-actor-server = servidor
admin-act-join = entró
admin-act-kick = expulsó
admin-act-disconnect = desconectó
admin-act-move = movió
admin-act-roles = asignó roles
admin-act-purge = purgó
admin-act-restrict = restringió
admin-act-lift = levantó una restricción
admin-act-message-delete = borró un mensaje
admin-act-invite-create = creó una invitación
admin-act-invite-revoke = revocó una invitación
admin-act-key-link = vinculó una clave
admin-act-key-unlink = desvinculó una clave
admin-act-profile-reset = restableció un perfil
admin-act-role-create = creó un rol
admin-act-role-edit = editó un rol
admin-act-role-delete = borró un rol
admin-act-channel-create = creó un canal
admin-act-channel-edit = editó un canal
admin-act-channel-delete = borró un canal
admin-act-blob-delete = borró un archivo
file-download-broken = La descarga de { $name } falló; probá de nuevo
file-open = Abrir
file-save-again = Descargar de nuevo
file-gone = Ese archivo ya no está en tu carpeta de Descargas
file-open-error = No se pudo abrir el archivo: { $error }
file-cancel = Cancelar
admin-channel = Registros de admin
admin-channel-lead = La auditoría, en vivo. Los comandos funcionan acá: /kick, /ban, /mute, /grant, /invite…
admin-channel-commands-only = Este canal solo acepta comandos — empezá con /

# ── Hosts de teléfono (ANDROID-SPEC.md, IOS-SPEC.md; una ventana por proceso) ──
quit-ios = En el iPhone las apps se cierran desde el selector de apps: deslizá hacia arriba desde el borde inferior, esperá un momento y deslizá baucord hacia arriba.
pick-from-photos = Fotos
pick-from-files = Archivos

# ── 1.2: bloquear, denunciar, borrar tu cuenta ────────────────────────
menu-block = Bloquear
menu-unblock = Desbloquear
menu-report = Denunciar…
menu-report-message = Denunciar mensaje…
menu-block-author = Bloquear a quien lo envió
dm-failed-unavailable = No podés escribirle a esta persona.
toast-blocked = Bloqueaste a { $name }. No vas a ver sus mensajes ni escuchar su voz.
toast-unblocked = Desbloqueaste a { $name }. Sus mensajes nuevos se ven; los anteriores vuelven cuando te reconectes.
report-title-user = Denunciar a { $name }
report-title-message = Denunciar un mensaje de { $name }
report-hint = Los moderadores del servidor ven esta denuncia y el mensaje tal como está ahora. A la persona no se le dice quién la denunció.
report-reason = Motivo
report-reason-placeholder = Elegí un motivo
report-reason-spam = Spam o estafa
report-reason-harassment = Acoso o bullying
report-reason-hate = Discurso de odio
report-reason-sexual = Contenido sexual
report-reason-violence = Violencia o amenazas
report-reason-self-harm = Autolesión o suicidio
report-reason-illegal = Algo ilegal
report-reason-other = Otra cosa
report-note = Algo para agregar
report-note-placeholder = Opcional
report-also-block = Bloquear también
report-also-block-hint = Dejar de ver sus mensajes y de escuchar su voz.
report-send = Enviar denuncia
report-sent = Denuncia enviada a los moderadores.
report-refused-no-message = Ese mensaje ya no está en el servidor.
report-refused-no-user = Esa cuenta ya no está en el servidor.
report-refused-yourself = No te podés denunciar a vos.
report-refused-too-many = Demasiadas denuncias en poco tiempo. Probá de nuevo en unos minutos.
blocked-header = Personas bloqueadas
blocked-not-connected = Conectate a un servidor para ver a quién bloqueaste ahí.
blocked-lead = No ves sus mensajes ni escuchás su voz, y no te pueden mandar mensajes directos. No se les avisa.
blocked-none = Nadie
unblock = Desbloquear
account-delete-header = Borrar tu cuenta
account-delete-lead = Tu cuenta en { $server }.
account-delete-note = Borra tu cuenta en este servidor y todo lo que publicaste ahí: mensajes, archivos, mensajes directos. No se puede deshacer. Para volver a entrar vas a necesitar una invitación nueva.
account-delete = Borrar mi cuenta
account-delete-confirm = Borrar todo — ¿seguro?
account-deleting = Borrando…
account-delete-last-admin = Sos el único admin de este servidor. Hacé admin a otra persona primero y después borrá tu cuenta.
account-deleted = Se borró tu cuenta en { $server }, con todo lo que publicaste ahí. Para volver a entrar vas a necesitar una invitación nueva.
admin-reports = Denuncias
admin-reports-lead = Lo que la gente denunció a los moderadores, lo más nuevo primero.
admin-reports-show-resolved = Mostrar denuncias atendidas
admin-reports-none = No hay denuncias abiertas.
admin-reports-by = Denunciado por
admin-reports-by-gone = Denunciado por una cuenta borrada
admin-reports-resolve = Marcar atendida
admin-reports-delete-message = Borrar mensaje
admin-reports-what-general = un mensaje en General
admin-reports-what-dm = un mensaje directo
admin-reports-what-person = la persona
admin-act-report-message = denunció un mensaje de
admin-act-report-user = denunció a
admin-act-report-resolve = atendió la denuncia
admin-act-user-delete = borró, a pedido de su dueño, la cuenta
chat-admin-reports-header = { $count } denuncias, las más nuevas primero
# After a report /reports lists that was already dealt with.
chat-admin-report-handled = atendida
chat-command-usage-resolve = Uso: /resolve <número de denuncia> (ver /reports)
# ── La línea de términos de la tarjeta de conexión (1.2) ──────────────
terms-notice = Al conectarte aceptás los
terms-link = términos y condiciones

# ── Actividad de General (1.2): el botón del encabezado, el vacío ─────
activity-show = Actividad
activity-hide = Volver a los mensajes
activity-empty-title = Todavía no hay actividad
activity-empty = Las entradas, salidas, cambios de sala y pantallas compartidas aparecen acá mientras estás conectado.

# ── Ajustes → Licencias de código abierto (1.2) ────────────────────────
tab-licenses = Licencias
licenses-title = Licencias de código abierto
licenses-lead = baucord es software libre bajo la Apache License 2.0, hecho sobre el software de código abierto de abajo. Cada parte mantiene su propia licencia; abrí una para leerla.
licenses-section-components = Componentes
licenses-section-crates = Crates de Rust ({ $count })
licenses-crates-count = { $count ->
    [one] 1 crate
   *[other] { $count } crates
}
licenses-used-by = Lo usan: { $crates }

# ── Lapsos de tiempo, y lo que el servidor rechazó (review P3-30) ─────
span-days = { $n } d
span-hours = { $n } h
span-minutes = { $n } min
span-seconds = { $n } s
span-days-hours = { $d } d { $h } h
span-hours-minutes = { $h } h { $m } min
span-ago = hace { $span }
share-denied = No se pudo compartir pantalla: { $why }
watch-denied = No se puede ver a { $name }: { $why }
deny-share-limit = el servidor llegó a su límite de pantallas compartidas
deny-viewer-limit = esta transmisión llegó a su límite de espectadores
deny-not-sharing = ese usuario no está compartiendo su pantalla
deny-dimensions = el servidor rechazó el tamaño de la transmisión
deny-not-in-channel = esta transmisión solo se ve desde su canal de voz
deny-not-permitted = no tenés permiso
channel-deny-missing = Ese canal ya no existe.
channel-deny-not-permitted = No tenés permiso para entrar a ese canal.
upload-deny-too-large = el archivo supera el tamaño que acepta este servidor
upload-deny-bad-type = solo se aceptan imágenes PNG, JPEG y WebP
upload-deny-damaged = el archivo llegó dañado
upload-deny-refused = el servidor rechazó el archivo
upload-deny-quota = ya usaste tu cupo de subidas de hoy
upload-deny-full = el servidor no tiene más lugar para archivos
upload-deny-not-permitted = no tenés permiso para subir archivos acá
channel-flag-afk = AFK
channel-flag-temporary = temporal
channel-flag-password = con contraseña
channel-flag-permanent = permanente
channel-list-limit = límite { $limit }
admin-list-no-roles = sin roles
admin-list-key = clave { $id }
admin-list-uses = usos { $uses }
admin-error-no-permission = no tenés permiso para eso
admin-error-no-user = no existe ese usuario
admin-error-no-restriction = no existe esa restricción
admin-error-no-invite = no existe esa invitación
admin-error-no-role = no existe ese rol
admin-error-no-channel = no existe ese canal
admin-error-outranked = tiene tu mismo rango o uno mayor
admin-error-last-admin = el último administrador sigue siendo administrador
admin-error-yourself = no sobre vos mismo
