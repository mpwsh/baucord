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
invite-code = Código de convite
invite-code-placeholder = ex. K7FQ-2MX9-ABCD — só é preciso na primeira vez
server-invite-note = Enviado uma vez, na sua primeira visita; depois a chave deste dispositivo é o login.
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
server-fingerprint = Identidade do servidor (SHA256:…, do convite; aprendida na primeira conexão)
server-config-note =
    A identidade é aprendida na primeira conexão e fixada; cole-a do log de inicialização do servidor para verificá-lo antes. Vale na próxima conexão.

# ── Veredictos de admissão (cartão de conexão) ──────────────────────────
connect-need-invite = Este servidor ainda não conhece este dispositivo. Digite o código de convite que você recebeu e aperte Conectar.
connect-invite-expired = Esse código de convite expirou — peça um novo.
connect-invite-revoked = Esse código de convite foi revogado — peça um novo.
connect-invite-exhausted = Esse código de convite já foi usado — peça um novo.
connect-invite-unknown = Esse não é um código de convite válido. Confira e tente de novo.
connect-key-retired = Este dispositivo foi desvinculado do seu usuário neste servidor. Peça a um admin para vinculá-lo de novo, ou regenere sua identidade em Configurações para começar como um novo usuário.
connect-name-in-use = Esse apelido já está em uso neste servidor agora — escolha outro.
error-invite-code-malformed = Isso não parece um código nem um link de convite.
error-no-identity = Não foi possível carregar sua chave de identidade ({ $path }). Corrija ou apague o arquivo e reinicie.
error-banned = Você está banido deste servidor{ $until }{ $reason }
ban-for = por { $left }
ban-indefinite = indefinidamente
removed-disconnected = Desconectado por { $by }: { $reason }
removed-kicked = Expulso por { $by }: { $reason } — você vai precisar de um novo convite para voltar.
removed-banned = Banido por { $by }{ $until }{ $reason }
removed-replaced = Você entrou de outro dispositivo ou conexão; esta foi encerrada.
chat-server-not-persistent = Este servidor não guarda nada entre reinícios: cargos, convites e banimentos duram até ele parar.

# ── Convites (/invite no compositor) ────────────────────────────────────
chat-invite-created = Código de convite { $code } (copiado) — { $role }, { $expiry }, { $uses }. Passe-o junto com o endereço e a porta do servidor.
chat-invite-usage = Uso: /invite [guest|member|organizer] [24h|7d] [usos]
chat-invite-no-permission = Você não pode criar esse convite aqui.
chat-invite-role-not-allowed = Códigos de convite não podem dar esse cargo.
invite-uses = { $count } uso(s)
invite-uses-unlimited = usos ilimitados
invite-expires-in = expira em { $left }
invite-never-expires = nunca expira

# ── Moderação (comandos; spec §4, §8) ───────────────────────────────────
chat-commands-help = Comandos: { $commands }
chat-command-unknown = Comando desconhecido /{ $command }. Tente /help.
chat-command-usage = Uso: /{ $command } nome …
chat-command-usage-revoke-invite = Uso: /revoke-invite <código>
chat-command-usage-unlink = Uso: /unlink <hash da chave, 64 dígitos hex> (veja /users)
chat-command-usage-link = Uso: /link <nome> <hash da chave, 64 dígitos hex>
chat-command-no-such-user = Ninguém online se chama { $name }.
chat-admin-refused = Recusado: { $why }.
chat-admin-done = Feito.
chat-server-muted-locked = O servidor silenciou você; poderá reativar o microfone quando isso for removido.
chat-your-roles-changed = Seus cargos agora são: { $roles }
roles-none = nenhum
reason-none = (sem motivo)
chat-admin-users-header = { $count } usuário(s) registrados (● online):
chat-admin-restrictions-header = { $count } restrição(ões):
chat-admin-invites-header = { $count } convite(s):
restricted-voice = O servidor silenciou você{ $until }{ $reason }
restricted-text = Você não pode escrever no chat{ $until }{ $reason }
restricted-dm = Você não pode enviar mensagens diretas{ $until }{ $reason }
restricted-upload = Você não pode enviar arquivos{ $until }{ $reason }
restricted-share = Você não pode compartilhar a tela{ $until }{ $reason }
restricted-profile = Você não pode alterar seu perfil{ $until }{ $reason }
restricted-server = Você não pode entrar{ $until }{ $reason }
lifted-voice = Seu silenciamento foi removido.
lifted-text = Você pode escrever no chat de novo.
lifted-dm = Você pode enviar mensagens diretas de novo.
lifted-upload = Você pode enviar arquivos de novo.
lifted-share = Você pode compartilhar a tela de novo.
lifted-profile = Você pode alterar seu perfil de novo.
lifted-server = Você pode entrar de novo.
restricted-by-role = seu cargo não permite

# ── Identidade (Configurações → Geral) ──────────────────────────────────
identity-header = Identidade
identity-export = Exportar
identity-import = Importar
identity-regenerate = Regenerar
identity-note = A chave deste dispositivo é quem você é em todos os servidores. Exporte-a para ser o mesmo usuário em outra máquina; Importar substitui a chave deste dispositivo; Regenerar cria uma nova (novo usuário — você vai precisar de um convite).
identity-missing = sem chave de identidade
identity-exported = Identidade exportada.
identity-imported = Identidade importada — agora você é esta chave. Reconecte para usá-la.
identity-regenerated = Nova identidade criada. A próxima conexão precisa de um convite.

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

# Resultados de conexão (1.0.1). Curtos, no idioma do usuário; o motivo
# técnico fica no baucord.log.
error-lost-timeout = Conexão perdida — o servidor não responde. Verifique sua internet.
error-lost-reset = Conexão perdida — o servidor encerrou a sessão depois que sua conexão ficou muda.
error-lost-unreachable = Não dá para alcançar o servidor agora — sua conexão está perdendo pacotes.
error-lost-generic = Conexão perdida.
error-lost-gave-up = A reconexão automática desistiu; clique em Conectar para tentar de novo.
error-reconnecting = Reconectando em { $secs } s…
error-refused-full = O servidor está cheio no momento.
error-refused-rate-limited = Muitas tentativas falhas — espere um minuto e tente de novo.
error-outdated = Esta versão do baucord não combina com o servidor. Atualize o baucord e tente de novo.
error-local = Não foi possível iniciar a conexão: { $detail }
error-lost-restart = O servidor está reiniciando.

# ── Histórico, progresso, canais (spec §5, §6, §9) ───────────────────────
chat-history-loaded = { $count } mensagem(ns) anteriores{ $more ->
    [yes] ; /history carrega mais
   *[no] .
}
chat-history-unavailable = O histórico não está disponível agora; tente de novo em { $secs } s.
chat-edited-mark = (editado)
chat-deleted-placeholder = Mensagem apagada
chat-message-edited = A mensagem #{ $id } foi editada: { $text }
chat-message-deleted = A mensagem #{ $id } foi apagada.
chat-rate-limited = Devagar: o chat tem limite; tente de novo em { $secs } s.
chat-level-up = Subiu de nível! Agora você é nível { $level } ({ $xp } xp).
chat-moved-idle = Você foi movido para o canal AFK após { $min } minutos sem atividade.
chat-command-usage-channel = Uso: /channel list · /channel create <nome> [senha] · /channel keep <nome> · /channel delete <nome> · /channel kick <usuário>
channel-owner-server = servidor
chat-admin-no-telemetry = Nada recebido dessa pessoa nesta execução.

# ── 1.1 menu da lista, insígnias, diálogo de moderação ──────────────────
menu-volume = Volume
menu-mute-for-me = Silenciar para mim
menu-unmute-for-me = Reativar som para mim
menu-role = Papel
menu-move-to = Mover para
menu-channel-kick = Mandar para o canal principal
menu-mute = Silenciar
menu-mute-voice = Voz…
menu-mute-chat = Chat…
menu-unmute-voice = Reativar a voz
menu-unmute-chat = Reativar o chat
menu-disconnect = Desconectar…
menu-kick = Expulsar…
menu-ban = Banir…
role-guest = Convidado
role-member = Membro
role-organizer = Organizador
role-admin = Admin
tooltip-level = Nível { $level }
card-xp-line = { $have } / { $need } xp para o nível { $next }
tooltip-telemetry-age = Telemetria { $ago }
tooltip-no-telemetry = Ainda não chegou telemetria dessa pessoa.
moderation-duration = Duração
moderation-reason = Motivo
moderation-reason-placeholder = Opcional — é mostrado à pessoa e fica no registro
server-not-persistent-note = Este servidor não guarda alterações. Papéis, banimentos e convites duram até ele reiniciar.
mod-title-mute-voice = Silenciar a voz de { $name }?
mod-title-mute-chat = Silenciar { $name } no chat?
mod-title-disconnect = Desconectar { $name }?
mod-title-kick = Expulsar { $name }?
mod-title-ban = Banir { $name }?
mod-hint-mute-voice = Continua conectado; o servidor descarta a voz até o silenciamento acabar.
mod-hint-mute-chat = Continua conectado; o servidor descarta mensagens e imagens até o silenciamento acabar.
mod-hint-disconnect = A conexão é fechada. Voltar é um clique.
mod-hint-kick = A conexão é fechada e este dispositivo é esquecido: precisa de um convite novo para voltar.
mod-hint-ban = A conexão é fechada, e o usuário, o dispositivo e o endereço são recusados até o banimento acabar.
mod-confirm-mute = Silenciar { $name }
mod-confirm-disconnect = Desconectar { $name }
mod-confirm-kick = Expulsar { $name }
mod-confirm-ban = Banir { $name }
duration-10m = 10 minutos
duration-30m = 30 minutos
duration-2h = 2 horas
duration-24h = 24 horas
duration-7d = 7 dias
duration-30d = 30 dias
duration-forever = Até ser removido

# ── 1.1 chat: editar / apagar / carregar anteriores ────────────────────
menu-edit-message = Editar mensagem
menu-delete-message = Apagar mensagem
chat-editing = Editando sua mensagem — Enter salva, Esc cancela
chat-load-earlier = Carregar mensagens anteriores
chat-loading = Carregando…
chat-no-earlier = Não há mensagens anteriores.
chat-edit-not-applied = O servidor não aplicou a edição — a mensagem pode ser velha demais para mudar.
chat-delete-not-applied = O servidor não apagou a mensagem — ela pode ser velha demais para mudar.
confirm-delete-message-title = Apagar esta mensagem?
confirm-delete-message-body = Ela é removida para todos.
confirm-delete-message-body-admin = Ela é removida para todos; o registro de auditoria guarda uma linha.
confirm-delete-message = Apagar mensagem

# ── 1.1 canais pela interface ─────────────────────────────────────────
channel-new = Novo canal
channel-name = Nome
channel-name-placeholder = Até 40 caracteres
channel-password-placeholder = Opcional
channel-limit = Limite de usuários
channel-limit-placeholder = Sem limite
channel-afk = AFK depois de (minutos)
channel-afk-placeholder = Padrão do servidor
channel-keep = Manter este canal
channel-keep-hint = Senão ele é removido depois de ficar vazio por um tempo.
channel-members-only = Só membros
channel-members-only-hint = Convidados não o veem nem podem entrar.
channel-create = Criar canal
channel-save = Salvar alterações
channel-not-persistent-note = Este servidor não guarda canais: ele dura até o servidor reiniciar.
channel-dialog-title-new = Novo canal
channel-dialog-title-edit = Editar { $name }
channel-password-unknown = Este canal tem uma senha que este dispositivo não conhece. Digite-a para mantê-la, ou deixe o campo vazio para removê-la.
channel-error-name = Dê ao canal um nome de até 40 caracteres.
channel-error-limit = O limite de usuários precisa ser um número inteiro de 1 ou mais, ou vazio.
channel-error-afk = Os minutos de AFK precisam ser um número inteiro de 1 ou mais, ou vazio.
channel-denied-name-taken = Já existe um canal com esse nome.
channel-denied-no-permission = Você não pode fazer isso com este canal.
channel-denied-config-owned = Esse canal está definido no arquivo de configuração do servidor e não pode ser alterado aqui.
channel-denied-limit = Você já tem tantos canais quanto este servidor permite.
channel-denied-no-such = Esse canal não existe mais.
channel-denied-invalid = O servidor não aceitou essas configurações.
menu-channel-edit = Editar canal…
menu-channel-delete = Apagar canal…
menu-channel-config-owned = Definido na configuração do servidor
confirm-delete-channel-title = Apagar { $name }?
confirm-delete-channel-body = Todos que estão nele vão para o canal principal.
confirm-delete-channel = Apagar canal

menu-select-text = Selecionar texto
menu-show-formatting = Mostrar formatação
channel-join = Entrar no canal

# ── Configurações: aba de perfil, adições em geral ─────────────────────
tab-profile = Perfil
identity-regenerate-confirm = Regenerar — usuário novo, tem certeza?
profile-lead = Como você aparece para os outros.
profile-no-name = Ainda sem apelido
profile-nickname-hint = Renomeia você para todos enquanto conectado.
profile-name-color = Cor do nome
profile-name-style = Estilo do nome
name-style-normal = Normal
name-style-bold = Negrito
name-style-italic = Itálico
profile-avatar-choose = Escolher imagem…
profile-avatar-remove = Remover
profile-avatar-error = Não deu para usar essa imagem: { $error }
profile-role-line = Em { $server }: { $role } · Nível { $level }
profile-role-line-no-level = Em { $server }: { $role }
profile-not-connected = Desconectado — seu papel aparece aqui quando você se conectar.
general-lead = Idioma, tamanho do texto e arquivos.
text-size = Tamanho do texto
text-size-hint = Vale em todo lugar, na hora.
text-size-small = Pequeno
text-size-default = Padrão
text-size-large = Grande
text-size-larger = Maior
sounds-header = Sons
sounds-lead = O que faz som, e quando. Mensagens comuns nunca fazem.
sounds-share-started = Alguém no seu canal começa a compartilhar
sounds-viewer-joined = Alguém começa a assistir sua transmissão
sounds-viewer-joined-hint = Mais suave, e só você ouve.
sounds-enabled = Ativar sons
sounds-enabled-hint = A chave geral: desligada, silencia todos os sons.
sounds-join-leave = Alguém entra ou sai do seu canal
files-header = Arquivos
files-note = As configurações, a chave de identidade e o avatar ficam nesta pasta.
servers-lead = Servidores salvos; o padrão é o que a tela de conexão abre.
audio-lead = Dispositivos, gate de entrada e nível de saída — cada mudança vale na hora.

# ── Configurações: inicialização, notificações, chat, privacidade, transmissão ─
sounds-mention = Uma mensagem cita você
sounds-mention-hint = Seu apelido como palavra, ou @apelido. Desligado por padrão: o chat geral pode ficar movimentado. Mensagens comuns nunca tocam som.
startup-header = Inicialização
startup-auto-connect = Conectar ao servidor padrão ao abrir
startup-auto-connect-hint = Precisa de um apelido; a tela de conexão é pulada.
startup-minimized = Iniciar na bandeja
startup-minimized-hint = A janela fica oculta até você abri-la pelo ícone da bandeja.
startup-check-updates = Verificar nova versão ao abrir
startup-check-updates-hint = Desligado, o app vai direto para a tela de conexão.
chat-header = Chat
chat-show-timestamps = Mostrar a hora em cada mensagem
chat-compact-rows = Linhas compactas
chat-image-size = Imagens que você envia
chat-image-size-hint = Imagens maiores são reduzidas para caber. Os servidores limitam uploads a 10 MB.
image-size-max = Máximo (10 MB)
privacy-header = Privacidade
privacy-note = Enquanto conectado, um servidor vê seu apelido, o id da sua chave, seus papéis e nível, em qual canal você está e, a cada 10 segundos, uma linha de telemetria: tempo de ida e volta, perda de pacotes, os nomes dos seus dispositivos de áudio e vídeo, a versão do app e o sistema operacional. Admins desse servidor podem ler a última linha na lista. Nada vai para outro lugar.
audio-transmission = Transmissão
audio-vad = Detecção de voz
audio-ptt = Apertar para falar
audio-vad-hint = Sua voz é enviada sempre que passa da sensibilidade abaixo.
audio-ptt-hint = Segure para falar:
audio-ptt-unbound = Nenhuma tecla ainda — defina uma na aba Teclado.
ptt-tooltip = Apertar para falar — segure

# ── Perfis na rede (spec §7.1) ────────────────────────────────────────
chat-user-renamed = { $old } agora é { $new }.
profile-denied-reserved-color = Nomes dourados são reservados para admins; escolha outra cor.
profile-denied-low-contrast = Essa cor não é legível sobre o fundo do chat; escolha outra.
profile-denied-rate-limited = Uma mudança de perfil a cada 30 segundos.
profile-denied-locked = Um admin bloqueou seu perfil.
profile-denied-bad-avatar = O servidor não aceitou essa imagem.
profile-avatar-refused = A imagem foi recusada: { $why }
rename-denied-taken = Esse nome está em uso aqui.
rename-denied-invalid = Esse nome está vazio.
rename-denied-rate-limited = Uma troca de nome a cada 30 segundos.
viewer-fullscreen = Tela cheia (F)
viewer-exit-fullscreen = Sair da tela cheia (Esc)

# ── Divisores de dia no chat ──────────────────────────────────────────
chat-day-today = Hoje
chat-day-yesterday = Ontem
chat-day-label = { $weekday }, { $day } de { $month }
chat-day-label-year = { $weekday }, { $day } de { $month } de { $year }
weekday-mon = segunda-feira
weekday-tue = terça-feira
weekday-wed = quarta-feira
weekday-thu = quinta-feira
weekday-fri = sexta-feira
weekday-sat = sábado
weekday-sun = domingo
month-1 = janeiro
month-2 = fevereiro
month-3 = março
month-4 = abril
month-5 = maio
month-6 = junho
month-7 = julho
month-8 = agosto
month-9 = setembro
month-10 = outubro
month-11 = novembro
month-12 = dezembro

# ── Mensagens diretas (spec §6) ───────────────────────────────────────
chat-dm-received = Mensagem direta de { $name }: { $text }
dm-failed-offline = Está offline, e este servidor não guarda mensagens.
dm-failed-no-permission = Mensagens diretas são para membros.
dm-failed-no-such-user = Esse usuário não existe.
dm-failed-store = O servidor não conseguiu guardar a mensagem. Tente de novo.

messages-header = Mensagens
voice-header = Voz
messages-collapse = Recolher mensagens
messages-expand = Expandir mensagens
invite-button = Convidar usuários
card-offline = Desconectado
invite-dialog-title = Convidar usuários
invite-role = Entra como
invite-expiry = Expira
invite-uses-label = Usos
invite-create = Criar convite
invite-copy = Copiar código
invite-close = Fechar
invite-copied = Código de convite copiado
invite-share-hint = Passe o código junto com o endereço e a porta do servidor.
invite-expiry-1h = 1 hora
invite-expiry-1d = 1 dia
invite-expiry-7d = 7 dias
invite-expiry-30d = 30 dias
invite-expiry-note = Mais de um dia exige a permissão de gerenciar convites; senão o servidor encurta.
invite-uses-1 = 1 uso
invite-uses-5 = 5 usos
invite-uses-10 = 10 usos
invite-uses-unlimited-option = Ilimitados
dm-general = Geral
dm-direct-message = Mensagem direta
dm-note-memory = Chega só enquanto a pessoa está online; este servidor não guarda nada.
dm-back = Voltar ao Geral
chat-placeholder-dm = Mensagem para
menu-message = Mensagem…
sounds-dm = Chega uma mensagem direta
chat-dm-with-offline = { $name } está offline; a mensagem espera no servidor.
chat-command-usage-dm = Uso: /dm <nome> <mensagem>
chat-dm-unknown = Ninguém aqui se chama { $name }.
chat-dm-no-images = Imagens em mensagens diretas chegam com a atualização de arquivos.
dm-close = Fechar conversa
