# ── Connect view ────────────────────────────────────────────────────────
app-tagline = Temos discord em casa
nickname = Apelido
connect = Conectar
settings = Configurações
quit = Sair
error-nickname-empty = O apelido não pode ficar vazio

# ── Missing-device warning ──────────────────────────────────────────────
device-missing-mic = microfone "{ $name }"
device-missing-output = saída "{ $name }"
device-warning = ⚠ { $devices } salvo(s) não conectado(s)
device-warning-hint = Abra as Configurações para escolher outro dispositivo
list-and = e

# ── Settings window ─────────────────────────────────────────────────────
settings-title = Configurações
tab-general = Geral
tab-audio = Áudio
tab-keyboard = Teclado
tab-credits = Créditos
tab-about = Sobre
save-and-close = Salvar e fechar
reset-defaults = Restaurar padrões

# ── General panel ───────────────────────────────────────────────────────
language = Idioma
language-system = Padrão do sistema
language-note =
    Aplicado imediatamente. Janelas já abertas serão atualizadas quando
    forem reabertas, e o histórico do chat mantém seu idioma original.
# ── Server config (connect + settings) ──────────────────────────────────
server-password = Senha do servidor (opcional)
settings-server = Servidor
tab-servers = Servidores
server-name = Nome
server-ip = IP / host
server-port = Porta
server-default = Padrão
server-add = Adicionar servidor
server-remove = Remover
server-manage = Gerenciar servidores
server-new-name = Novo servidor
save = Salvar
connect-access-denied = Acesso negado: senha — digite abaixo, Salve e Conecte de novo
server-fingerprint = Identidade do servidor (SHA256:…, do convite; aprendida na primeira conexão)
server-config-note =
    A senha é o segredo da sala que todos no seu servidor compartilham. A identidade é aprendida na primeira conexão e fixada; cole-a de um convite para verificar o servidor antes. Ambas valem na próxima conexão.

# ── Sidebar ─────────────────────────────────────────────────────────────
users-count = Usuários: { $count }
status-connecting = Conectando...
status-connected = Conectado
status-disconnected = Desconectado
mute = Silenciar
unmute = Ativar som
deafen = Ensurdecer
undeafen = Reativar áudio
share-screen = Compartilhar tela
stop-sharing = Parar compartilhamento
disconnect = Desconectar
user-volume = Volume do usuário
reset-audio = Redefinir áudio
watch-stream = Assistir transmissão
live = Ao vivo
stream-stats-quality = { $quality } · { $viewers } assistindo
stream-stats-frame = { $kb }KB · { $ms }ms cod
stream-stats-degraded = { $fps }% fps · perda { $loss }%
latency-line = Áudio: { $audio } | Rede: { $network } | ↑{ $up } ↓{ $down } Mb

# ── Chat ────────────────────────────────────────────────────────────────
send = Enviar
chat-disconnected = Desconectado: { $reason }
disconnect-reason-user = Usuário desconectado
chat-user-joined = { $name } entrou
chat-user-left = { $name } saiu
chat-share-started = Compartilhamento de tela iniciado ({ $quality })
chat-share-stopped = Compartilhamento de tela encerrado
chat-peer-sharing = { $name } começou a compartilhar a tela
chat-peer-stopped-sharing = { $name } parou de compartilhar a tela
chat-watching = Assistindo à tela de { $name }...
chat-stopped-watching = Parou de assistir a { $name }.
chat-mic-connected = Microfone conectado: { $name }
chat-mic-disconnected = Microfone desconectado: { $name }
chat-speaker-connected = Alto-falante conectado: { $name }
chat-speaker-disconnected = Alto-falante desconectado: { $name }
chat-input-lost = Dispositivo de entrada perdido — alternando para o padrão do sistema
chat-input-default-changed = Entrada padrão alterada para: { $name }
chat-output-lost = Dispositivo de saída perdido — alternando para o padrão do sistema
chat-output-default-changed = Saída padrão alterada para: { $name }
unknown-device = dispositivo desconhecido

# ── Tray & viewer ───────────────────────────────────────────────────────
tray-show = Mostrar
tray-quit = Sair
viewer-title = Tela de { $name } ({ $width }x{ $height })
stop-watching = Parar de assistir
viewer-stats = { $res } { $fps }fps { $kbps }KB/s { $mb }MB | buf { $depth }/{ $target }ms jit { $jitter }ms

# ── Audio panel ─────────────────────────────────────────────────────────
audio-devices-header = DISPOSITIVOS
audio-input-device = Dispositivo de entrada (microfone)
audio-output-device = Dispositivo de saída (alto-falantes/fones)
audio-input-header = ENTRADA
audio-capture-level = Nível de captura
audio-input-gain = Ganho de entrada
audio-input-sensitivity = Sensibilidade de entrada
audio-gate-hint = Áudio abaixo da linha vermelha não será transmitido
audio-test-mic = Testar microfone
audio-stop-testing = Parar teste
audio-test-hint = Ouça seu próprio microfone (outras pessoas não vão ouvir você)
audio-testing-hint = Ouvindo seu microfone... outras pessoas não podem ouvir você
audio-output-header = SAÍDA
audio-master-volume = Volume principal
audio-system-default = Padrão do sistema
audio-device-count = { $inputs } dispositivo(s) de entrada, { $outputs } de saída
audio-gain-value = Ganho: { $percent }%
audio-volume-value = Volume: { $percent }%
audio-sensitivity-line = Sensibilidade: { $name }  (limiar: { $threshold } RMS)
sensitivity-very-high = Muito alta
sensitivity-high = Alta
sensitivity-medium = Média
sensitivity-low = Baixa
sensitivity-very-low = Muito baixa

# ── Keybind actions ─────────────────────────────────────────────────────
action-toggle-mute = Alternar silenciamento
action-toggle-deafen = Alternar ensurdecimento
action-toggle-share = Alternar compartilhamento de tela
action-push-to-talk = Pressionar para falar

# ── Keybinds panel ──────────────────────────────────────────────────────
keybinds-header = ATALHOS DE TECLADO
keybinds-hint = Clique em Gravar e pressione uma combinação de teclas. Esc cancela, Backspace limpa.
keybind-not-set = Não definido
keybind-record = Gravar
keybind-press-combo = Pressione uma combinação de teclas...

# ── Screen share dialog ─────────────────────────────────────────────────
monitor = Monitor
monitor-primary = (principal)
quality = Qualidade
start-sharing = Iniciar compartilhamento
error-no-monitors = Nenhum monitor encontrado

# ── Credits panel ───────────────────────────────────────────────────────
credits-header = CRÉDITOS
credits-super-testers = SUPER BETA TESTERS
credits-beta-testers = BETA TESTERS

# ── About panel ─────────────────────────────────────────────────────────
about-header = SOBRE
about-running-from = Executando de:
about-config-file = Arquivo de configuração:
about-open-config = Abrir pasta de configuração
about-open-folder = Abrir pasta de instalação
about-built-with = FEITO COM
tech-rust = Rust - Linguagem de programação
tech-fltk = FLTK - Interface de usuário
tech-quinn = quinn-proto - Transporte QUIC
tech-ffmpeg = FFmpeg - Codificação/decodificação de vídeo
tech-xcap = Xcap - Captura de tela
tech-cpal = CPAL - E/S de áudio
tech-opus = Opus - Codec de áudio
tech-rnnoise = RNNoise - Supressão de ruído
tech-rubato = Rubato - Reamostragem de áudio

# ── Update flow ─────────────────────────────────────────────────────────
splash-checking = Verificando atualizações...
update-available-title = Atualização disponível
update-available-heading = Nova versão disponível!
update-version-info =
    Sua versão: { $current }
    Versão mais recente: { $latest }
update-skip = Pular
update-now = Atualizar
update-downloading-title = Atualizando
update-downloading-heading = Baixando atualização v{ $version }...
update-starting-download = Iniciando download...
update-progress = Baixando... { $percent }%
update-ready-title = Atualização pronta
update-downloaded-heading = Atualização baixada!
update-ready-info = Pronto para instalar. O aplicativo será reiniciado automaticamente.
update-not-now = Agora não
# Rendered in an FLTK label — the code escapes '&' before display.
update-install-restart = Instalar e reiniciar
update-failed-title = Falha na atualização
update-failed-heading = Falha no download
update-continue = Continuar
cancel = Cancelar

# ── Errors & shared ─────────────────────────────────────────────────────
error-invalid-address = Endereço inválido (use ip:porta)
error-screen-share = Compartilhamento de tela: { $error }
fallback-username = Usuário{ $id }
unknown = Desconhecido

# ── Chat input, image attach & context menus ────────────────────────────
chat-emoji-tooltip = Emojis — ou digite códigos :nome: como :joy:
chat-attach-tooltip = Envie uma imagem do seu computador
chat-clear-tooltip = Limpar o chat (só para você)
chat-image-filter = Imagens
chat-image-load-error = Não foi possível carregar a imagem: { $error }
chat-image-undisplayable = { $name } enviou uma imagem que não pôde ser exibida
image-preview-title = Enviar imagem
image-save-filter = Imagem PNG
image-window-title = { $name } — imagem
menu-cut = Recortar
menu-copy = Copiar
menu-paste = Colar
menu-copy-message = Copiar mensagem
menu-copy-link = Copiar link
menu-open-link = Abrir link
menu-open-image = Abrir imagem
menu-save-image = Salvar imagem…

# ── Resource stats (sidebar) ────────────────────────────────────────────
resource-line = CPU { $cpu }% · Mem { $mem } MB

# ── GIFs & media caps ───────────────────────────────────────────────────
chat-gif-too-large = O GIF ultrapassa o limite de { $mb } MB
chat-gif-too-big = GIFs animados podem ter no máximo { $dim } px de largura ou altura
image-save-filter-gif = Imagem GIF
chat-upload-progress = Enviando imagem… { $pct }%

# ── Adições da migração para Slint ──────────────────────────────────────
tech-slint = Slint - Interface de usuário
chat-image-too-large = A imagem é grande demais para enviar ({ $kb } KB após redimensionar)
chat-image-save-error = Não foi possível salvar a imagem: { $error }
chat-downloading-image = Baixando imagem…

# ── Canais de voz ──
channel-password-title = Senha do canal
channel-wrong-password = Senha incorreta — tente de novo
channel-full = Esse canal está cheio
share-visibility = Visível para
share-channel-only = Meu canal
share-everyone = Todos
chat-channels-updated = os canais de voz foram atualizados

# Fixação da identidade do servidor (1.0)
error-identity-changed = A identidade do servidor mudou: esperado { $expected }, recebido { $observed }. Nada foi enviado. Se o administrador trocou a chave de propósito, edite a identidade do servidor nas Configurações (ou limpe-a) e conecte novamente.
chat-identity-pinned = Identidade do servidor fixada: { $fingerprint }
