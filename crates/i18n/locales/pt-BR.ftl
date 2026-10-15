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
device-warning = { $devices } salvo(s) não conectado(s)
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
# Settings' footer: leave without keeping what was changed since it opened.
settings-close = Fechar
reset-defaults = Restaurar padrões
reset-confirm-body = Todas as configurações voltam ao padrão, inclusive seus servidores salvos, apelido e foto.
reset-confirm = Restaurar tudo

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
banner-not-persistent = Este servidor não guarda nada entre reinícios: cargos, convites e banimentos duram até ele parar.

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
activity-command-done = { $command }: feito
chat-server-muted-locked = O servidor silenciou você; poderá reativar o microfone quando isso for removido.
chat-your-roles-changed = Seus cargos agora são: { $roles }
roles-none = nenhum
reason-none = (sem motivo)
chat-admin-users-header = { $count } usuário(s) registrados:
chat-admin-list-more = … e mais { $count }. O painel de administração mostra todos.
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
identity-imported = Identidade importada. Reconecte para usá-la.
identity-regenerated = Nova identidade criada. A próxima conexão precisa de um convite.

# ── Sidebar ─────────────────────────────────────────────────────────────
users-count = Usuários: { $count }
status-connected = Conectado
status-disconnected = Desconectado
mute = Silenciar
unmute = Ativar som
deafen = Ensurdecer
undeafen = Reativar áudio
share-screen = Compartilhar tela
stop-sharing = Parar compartilhamento
disconnect = Desconectar
reset-audio = Redefinir áudio
watch-stream = Assistir transmissão
live = Ao vivo
stream-stats-quality = { $quality } · { $viewers } assistindo
stream-stats-frame = { $kb }KB · { $ms }ms cod
stream-stats-degraded = { $fps }% fps · perda { $loss }%
latency-line = Áudio: { $audio } | Rede: { $network }
# The transfer rates' unit, after the download rate (the arrows are drawn).
latency-unit = Mb

# ── Chat ────────────────────────────────────────────────────────────────
send = Enviar
chat-disconnected = Desconectado: { $reason }
disconnect-reason-user = Usuário desconectado
chat-user-joined = { $name } entrou
chat-user-joined-channel = { $name } entrou em { $channel }
chat-user-moved = { $name } passou para { $channel }
chat-you-moved = Você entrou em { $channel }
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
tray-tip-muted = baucord — silenciado
tray-tip-deafened = baucord — ensurdecido
# In the tray's menu, one item per person in our voice room: muting is for you alone.
tray-mute-person = Silenciar { $name }
tray-unmute-person = Reativar som de { $name }
viewer-title = Tela de { $name } ({ $width }x{ $height })
stop-watching = Parar de assistir
viewer-stats = { $res } { $fps }fps { $kbps }KB/s { $mb }MB | buf { $depth }/{ $target }ms jit { $jitter }ms

# ── Audio panel ─────────────────────────────────────────────────────────
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
audio-testing-hint-delayed = Diga algo: você vai ouvir meio segundo depois. Outras pessoas não podem ouvir você
audio-output-header = SAÍDA
audio-master-volume = Volume principal
audio-system-default = Padrão do sistema
audio-output-device-android = Tocar o som no
audio-output-hint-android = Chamadas e som das transmissões. No automático usa o fone quando há um conectado; senão, o alto-falante.
audio-route-auto = Automático
audio-route-speaker = Alto-falante
audio-route-earpiece = Fone do celular
audio-route-wired = Fone com fio
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
keybinds-hint = Clique em Gravar e pressione uma combinação de teclas. Esc cancela, Backspace limpa.
keybinds-access = O macOS só deixa o baucord ver seus atalhos enquanto você está em outro app com a permissão de Acessibilidade. Até lá, uma tecla atribuída continua funcionando, mas o macOS a tira do app em que você está: ela não digita nada ali.
keybinds-access-ask = Permitir nos Ajustes do Sistema…
keybind-not-set = Não definido
keybind-press-combo = Pressione uma combinação de teclas...

# ── Screen share dialog ─────────────────────────────────────────────────
monitor = Monitor
monitor-primary = (principal)
quality = Qualidade
start-sharing = Iniciar compartilhamento
error-no-monitors = Nenhum monitor encontrado

# ── Credits panel ───────────────────────────────────────────────────────
credits-super-testers = SUPER BETA TESTERS
credits-beta-testers = BETA TESTERS

# ── About panel ─────────────────────────────────────────────────────────
about-running-from = Executando de:
about-config-file = Arquivo de configuração:
about-open-config = Abrir pasta de configuração
about-built-with = FEITO COM
tech-rust = Rust - Linguagem de programação
tech-quinn = quinn-proto - Transporte QUIC
tech-ffmpeg = FFmpeg - Codificação/decodificação de vídeo
tech-xcap = Xcap - Captura de tela
tech-cpal = CPAL - E/S de áudio
tech-opus = Opus - Codec de áudio
tech-rnnoise = RNNoise - Supressão de ruído
tech-rubato = Rubato - Reamostragem de áudio

# ── Update flow ─────────────────────────────────────────────────────────
splash-checking = Verificando atualizações...
update-available-heading = Nova versão disponível!
update-version-info =
    Sua versão: { $current }
    Versão mais recente: { $latest }
update-skip = Pular
update-now = Atualizar
update-downloading-heading = Baixando atualização v{ $version }...
update-progress = Baixando... { $percent }%
update-downloaded-heading = Atualização baixada!
update-ready-info = Pronto para instalar. O aplicativo será reiniciado automaticamente.
update-not-now = Agora não
# Rendered in an FLTK label — the code escapes '&' before display.
update-install-restart = Instalar e reiniciar
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
chat-image-load-error = Não foi possível carregar a imagem: { $error }
chat-image-undisplayable = { $name } enviou uma imagem que não pôde ser exibida
image-preview-title = Enviar imagem
image-save-filter = Imagem PNG
menu-cut = Recortar
menu-copy = Copiar
menu-paste = Colar
menu-copy-message = Copiar mensagem
menu-open-image = Abrir imagem
menu-save-image = Salvar imagem…
menu-copy-image = Copiar imagem

# ── Resource stats (sidebar) ────────────────────────────────────────────
resource-line = CPU { $cpu }% · Mem { $mem } MB

# ── GIFs & media caps ───────────────────────────────────────────────────
chat-gif-too-large = O GIF ultrapassa o limite de { $mb } MB
chat-gif-too-big = GIFs animados podem ter no máximo { $dim } px de largura ou altura
image-save-filter-gif = Imagem GIF
image-save-filter-jpeg = Imagem JPEG

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
# The admin panel's Actions menu (Users tab): each asks first.
menu-reset-profile = Redefinir perfil…
menu-purge = Purgar…
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
chat-new-messages = { $count ->
    [one] 1 mensagem nova
   *[other] { $count } mensagens novas
}
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
log-level = Nível de log
log-level-hint = Quanto o baucord.log registra. Debug ao relatar um problema.
log-level-error = Só erros
log-level-warn = Avisos
log-level-info = Info (padrão)
log-level-debug = Debug
log-level-trace = Trace (tudo)
tab-debug = Depuração
debug-lead = O que o app vem registrando, e jeitos de compartilhar isso quando algo dá errado: copiar ou mandar para o admin do servidor.
debug-log-header = Registro
debug-send-log = Enviar registro ao servidor
debug-copy-log = Copiar registro
debug-clear-log = Limpar registro
debug-send-hint = Copiar coloca o final do registro na área de transferência. Enviar manda até 2 MB dele ao servidor em que você está conectado, uma vez a cada 10 minutos; o admin dele o encontra na pasta logs do servidor.
debug-tail-label = Últimas 100 linhas
debug-tail-empty = O registro está vazio.
debug-no-log-file = Esta versão escreve o registro no terminal, não em um arquivo.
debug-log-file = Arquivo de registro: { $path } ({ $size })
debug-sent = Registro enviado para { $server } ({ $size }).
debug-send-wait = Já enviado: o servidor aceita um a cada 10 minutos. Tente de novo em { $minutes } min.
debug-send-offline = Conecte-se a um servidor primeiro.
debug-send-empty = Nada para enviar: o registro está vazio.
debug-read-failed = Não foi possível ler o registro: { $error }
debug-cleared = Registro limpo.
debug-clear-failed = Não foi possível limpar o registro: { $error }
debug-copied = Final do registro copiado ({ $size }).
debug-copy-empty = Nada para copiar: o registro está vazio.
debug-copy-failed = Não foi possível copiar o registro: { $error }
text-size-small = Pequeno
text-size-default = Padrão
text-size-large = Grande
text-size-larger = Maior
sounds-header = Sons
sounds-lead = Quais eventos tocam um som.
sounds-share-started = Alguém no seu canal começa a compartilhar
sounds-viewer-joined = Alguém começa a assistir sua transmissão
sounds-viewer-joined-hint = Mais suave, e só você ouve.
sounds-mute-toggle = Você se silencia ou ensurdece
sounds-mute-toggle-hint = Uma nota suave: mais grave ao silenciar, mais aguda ao voltar.
sounds-enabled = Ativar sons
sounds-enabled-hint = Desligado, silencia todos os sons.
sounds-join-leave = Alguém entra ou sai do seu canal
files-header = Arquivos
files-note = As configurações, a chave de identidade e o avatar ficam nesta pasta.
servers-lead = Servidores salvos; o padrão é o que a tela de conexão abre.
audio-lead = As mudanças valem na hora.

# ── Configurações: inicialização, notificações, chat, privacidade, transmissão ─
sounds-mention = Uma mensagem cita você
sounds-mention-hint = Seu apelido como palavra inteira, ou @apelido.
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
privacy-note = Enquanto conectado, um servidor vê seu apelido, o id da sua chave, seus papéis e nível, em qual canal você está e, a cada 10 segundos, uma linha de telemetria: tempo de ida e volta, perda de pacotes, os nomes dos seus dispositivos de áudio e vídeo, a versão do app e o sistema operacional. Admins desse servidor podem ler a última linha na lista.
audio-transmission = Transmissão
audio-vad = Detecção de voz
audio-ptt = Apertar para falar
audio-vad-hint = Sua voz é enviada sempre que passa da sensibilidade abaixo.
audio-ptt-hint = Segure para falar:
audio-ptt-unbound = Nenhuma tecla ainda — defina uma na aba Teclado.
audio-aec = Cancelamento de eco
audio-aec-hint = Evita que os outros se ouçam pelos seus alto-falantes. No automático, fones de ouvido ficam como estão.
audio-aec-hint-windows = Evita que os outros se ouçam pelos seus alto-falantes. No automático, fones de ouvido ficam como estão. Nos alto-falantes, o Windows pode abaixar outros apps enquanto você fala (Configurações de som → Comunicações).
audio-aec-hint-android = Evita que os outros se ouçam pelo alto-falante. No automático, fones de ouvido ficam como estão. No alto-falante, o celular trata a voz como uma chamada: os botões de volume ajustam o volume da chamada.
audio-aec-hint-ios = Evita que os outros se ouçam pelo alto-falante. No automático, fones de ouvido ficam como estão. No alto-falante, o iPhone trata a voz como uma chamada: os botões de volume ajustam o volume da chamada e os outros apps tocam um pouco mais baixo.
audio-aec-auto = Automático (só alto-falantes)
audio-aec-always = Sempre
audio-aec-off = Desligado
aec-status-on = Ligado — o cancelamento de eco do seu dispositivo está funcionando.
aec-status-headphones = Desligado — você está de fones de ouvido.
aec-status-off = Desligado.
aec-status-no-canceller = Indisponível — este dispositivo não tem cancelamento de eco.
aec-status-canceller-off = Indisponível — não foi possível ligar o cancelamento de eco. Os detalhes estão no baucord.log.
aec-status-refused = Indisponível — o sistema não abriu o áudio de chamada. Os detalhes estão no baucord.log.
aec-status-unsupported = Ainda não disponível neste sistema.
aec-status-idle = Vale quando você entrar num canal de voz.
aec-lowering = O Windows abaixa o volume dos seus outros apps enquanto o baucord está conectado, e não deixou o baucord desativar isso. Nas configurações de som do Windows, na aba Comunicações, escolha "Não fazer nada".
aec-lowering-fix = Abrir as configurações de som do Windows
aec-lowering-told = O Windows abaixa o volume dos seus outros apps enquanto o baucord está conectado. Em Configurações → Áudio você vê como evitar.
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
viewer-fill = Ampliar para preencher (toque duplo)
viewer-fit = Ajustar à tela (toque duplo)
viewer-rotate = Girar para paisagem
viewer-unrotate = Voltar para retrato
viewer-mic-mute = Silenciar seu microfone
viewer-mic-unmute = Ativar seu microfone
viewer-stream-mute = Silenciar a transmissão
viewer-stream-unmute = Ativar o som da transmissão

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
dm-failed-offline = Está offline, e este servidor não guarda mensagens.
dm-failed-no-permission = Mensagens diretas são para membros.
dm-failed-no-such-user = Esse usuário não existe.
dm-failed-store = O servidor não conseguiu guardar a mensagem. Tente de novo.

messages-header = Mensagens
voice-header = Voz
lobby-banner = Entre em um canal de voz para começar a falar
drop-to-send = Solte para enviar
lobby-mic-hint = Entre em um canal para falar
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
menu-save-file = Salvar arquivo…
file-save = Salvar
file-uploading-pct = Enviando
file-downloading-pct = Baixando
file-upload-failed = { $name } não foi enviado: { $why }
file-too-large = { $name } tem mais de { $max } MB
file-read-error = Não foi possível ler { $name }: { $error }
notif-channel-voice = Conexão de voz
notif-connected-title = Conectado a { $server }
notif-connected-text = Toque para voltar ao app.
notif-channel-messages = Mensagens
notif-channel-activity = Atividade de voz
notif-dm-file = Enviou um arquivo: { $name }
notif-first-join-title = { $name } entrou em { $channel }
notif-first-join-text = A primeira pessoa na voz em { $server }.
notifs-header = Notificações
notifs-lead = Enquanto o baucord está em segundo plano e ainda conectado. Os modos silencioso e Não perturbe do celular valem aqui, e também para os sons de mensagens acima.
notifs-messages = Mensagens diretas
notifs-messages-hint = A mensagem e quem enviou; toque para abrir a conversa.
notifs-first-join = Alguém começa uma chamada
notifs-first-join-hint = A primeira pessoa a entrar em um canal de voz quando estão todos vazios. No máximo uma vez a cada 10 minutos.
file-pick-busy = Ainda lendo o último arquivo que você escolheu. Tente de novo em instantes.
file-saved = Salvo em { $path }
file-save-error = Não foi possível salvar { $name }: { $error }
file-download-failed = { $name } não está mais no servidor
dm-failed-no-such-blob = O arquivo não está no servidor
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
chat-empty-title = Nenhuma mensagem ainda
chat-empty-general = Diga algo para todos neste servidor.
chat-empty-dm = Diga oi para começar a conversa.
dm-general = Geral
dm-direct-message = Mensagem direta
dm-note-memory = Chega só enquanto a pessoa está online; este servidor não guarda nada.
dm-back = Voltar ao Geral
chat-placeholder-dm = Mensagem para
menu-message = Mensagem…
sounds-dm = Chega uma mensagem direta
chat-command-usage-dm = Uso: /dm <nome> <mensagem>
chat-dm-unknown = Ninguém aqui se chama { $name }.

# ── Admin panel (spec §8) ──
admin-title = Administração
admin-button = Admin
admin-refresh = Atualizar
admin-loading = Carregando…
admin-none = Nada aqui.
admin-users = Usuários
admin-users-lead = Todos que o servidor conhece, online ou não. As ações ficam na auditoria com o motivo.
admin-reason = Motivo
admin-reason-placeholder = Mostrado a eles e guardado na auditoria
admin-disconnect = Desconectar
admin-kick = Expulsar
admin-ban = Banir
admin-reset-profile = Redefinir perfil
admin-purge = Purgar
admin-set-role = Definir papel…
# A person's row in the admin panel: the button that opens their actions.
admin-actions = Ações
admin-roles = Papéis
admin-roles-lead = Conjuntos de permissões. A posição de um papel segue o que ele pode fazer.
admin-holders = titular(es)
admin-builtin = integrado
admin-edit = Editar
admin-delete = Excluir
admin-role-new = Novo papel
admin-role-editing = Editar papel
admin-role-name = Nome
admin-role-name-hint = 1–32 letras minúsculas, dígitos e hifens.
admin-role-rank-hint = A posição do papel na moderação segue suas permissões: tudo que um membro tem fica acima dos membros, tudo que um organizador tem fica acima dos organizadores.
admin-role-save = Salvar papel
admin-restrictions = Restrições
admin-restrictions-lead = Banimentos, silenciamentos e bloqueios em vigor.
admin-lift = Remover
admin-invites = Convites
admin-invites-lead = Códigos que dão entrada, e quantas vezes foram usados.
admin-revoke = Revogar
admin-revoked = revogado
admin-channels = Canais
admin-channels-lead = Canais de voz; os do arquivo de configuração não podem ser excluídos aqui.
admin-members = membro(s)
admin-config-owned = do arquivo de configuração
admin-files = Arquivos
admin-files-lead = Tudo guardado no servidor, com quem enviou.
admin-refs = ref(s)
admin-audit = Auditoria
admin-audit-lead = Quem fez o quê, mais novo primeiro.
admin-audit-more = Carregar anteriores
admin-server = Servidor
admin-server-lead = Os contadores do servidor.
admin-stat-version = Versão
admin-stat-uptime = No ar há
admin-stat-online = Online
admin-stat-users = Usuários conhecidos
admin-stat-messages = Mensagens
admin-stat-blobs = Arquivos guardados
admin-stat-persistence = Persistência
admin-stat-queue = Fila de escrita
admin-stat-dropped = Escritas perdidas
admin-stat-errors = Erros de escrita
admin-yours = seu
admin-now = agora
admin-storage = { $used } de { $cap } usados
admin-blobs-line = { $count } arquivos, { $used } de { $cap }
admin-persistent-on = ligada (SQLite)
admin-persistent-off = desligada (só memória)
admin-standing-admin = admin
admin-standing-above-organizers = acima dos organizadores
admin-standing-above-members = acima dos membros
admin-standing-above-guests = acima dos convidados
admin-role-badge = Distintivo
admin-confirm-disconnect = Desconectar { $name }?
admin-confirm-disconnect-text = A pessoa pode reconectar em seguida. O motivo é mostrado a ela e fica na auditoria.
admin-confirm-kick = Expulsar { $name }?
admin-confirm-kick-text = As chaves e os papéis são esquecidos: será preciso um novo convite para voltar. O motivo é mostrado à pessoa e fica na auditoria.
admin-confirm-ban = Banir { $name }?
admin-confirm-ban-text = A pessoa é desconectada e recusada daqui em diante, por identidade e endereço, até o banimento ser removido. O motivo é mostrado a ela e fica na auditoria.
admin-confirm-purge = Purgar { $name }?
admin-confirm-purge-text = Apaga o usuário por completo — conta, chaves, papéis e cada mensagem que escreveu — como se nunca tivesse entrado. Não dá para desfazer. É para fantasmas e spam; se a pessoa pode voltar, prefira banir.
admin-confirm-reset-profile = Redefinir o perfil de { $name }?
admin-confirm-reset-profile-text = Limpa a cor e o estilo do nome e a foto. A pessoa pode definir de novo depois.
admin-confirm-delete-role = Excluir o papel { $name }?
admin-confirm-delete-role-text = Quem o tem perde agora; os outros papéis ficam.
admin-confirm-delete-channel = Excluir o canal { $name }?
admin-confirm-delete-channel-text = Quem está nele vai para o canal padrão.
admin-confirm-delete-file = Excluir este arquivo?
admin-confirm-delete-file-text = É removido do servidor; as mensagens que o publicaram mantêm o cartão, mas Salvar dirá que ele se foi.
admin-confirm-revoke = Revogar o convite { $code }?
admin-confirm-revoke-text = O código deixa de admitir alguém. Quem já entrou fica.
admin-confirm-lift = Remover esta restrição?
admin-confirm-lift-text = Termina agora, e a auditoria registra quem removeu.
admin-col-when = QUANDO
admin-col-who = QUEM
admin-col-what = O QUÊ
admin-col-target = EM
admin-col-detail = DETALHES
admin-actor-server = servidor
admin-act-join = entrou
admin-act-kick = expulsou
admin-act-disconnect = desconectou
admin-act-move = moveu
admin-act-roles = definiu papéis
admin-act-purge = purgou
admin-act-restrict = restringiu
admin-act-lift = removeu uma restrição
admin-act-message-delete = apagou uma mensagem
admin-act-invite-create = criou um convite
admin-act-invite-revoke = revogou um convite
admin-act-key-link = vinculou uma chave
admin-act-key-unlink = desvinculou uma chave
admin-act-profile-reset = redefiniu um perfil
admin-act-role-create = criou um papel
admin-act-role-edit = editou um papel
admin-act-role-delete = excluiu um papel
admin-act-channel-create = criou um canal
admin-act-channel-edit = editou um canal
admin-act-channel-delete = excluiu um canal
admin-act-blob-delete = excluiu um arquivo
file-download-broken = O download de { $name } falhou; tente de novo
file-open = Abrir
file-save-again = Baixar de novo
file-gone = Esse arquivo não está mais na sua pasta Downloads
file-open-error = Não foi possível abrir o arquivo: { $error }
file-cancel = Cancelar
admin-channel = Registros de admin
admin-channel-lead = A auditoria, ao vivo. Os comandos funcionam aqui: /kick, /ban, /mute, /grant, /invite…
admin-channel-commands-only = Este canal só aceita comandos — comece com /

# ── Hosts de celular (ANDROID-SPEC.md, IOS-SPEC.md; uma janela por processo) ──
quit-ios = No iPhone, os apps são fechados pelo seletor de apps: deslize para cima a partir da borda inferior, pare um instante e deslize o baucord para cima.
pick-from-photos = Fotos
pick-from-files = Arquivos

# ── 1.2: bloquear, denunciar, excluir sua conta ───────────────────────
menu-block = Bloquear
menu-unblock = Desbloquear
menu-report = Denunciar…
menu-report-message = Denunciar mensagem…
menu-block-author = Bloquear quem enviou
dm-failed-unavailable = Você não pode enviar mensagens para esta pessoa.
toast-blocked = Você bloqueou { $name }. Você não vai ver as mensagens nem ouvir essa pessoa.
toast-unblocked = Você desbloqueou { $name }. As mensagens novas aparecem; as anteriores voltam quando você reconectar.
report-title-user = Denunciar { $name }
report-title-message = Denunciar uma mensagem de { $name }
report-hint = Os moderadores do servidor veem esta denúncia e a mensagem como está agora. A pessoa não fica sabendo quem a denunciou.
report-reason = Motivo
report-reason-placeholder = Escolha um motivo
report-reason-spam = Spam ou golpe
report-reason-harassment = Assédio ou bullying
report-reason-hate = Discurso de ódio
report-reason-sexual = Conteúdo sexual
report-reason-violence = Violência ou ameaças
report-reason-self-harm = Automutilação ou suicídio
report-reason-illegal = Algo ilegal
report-reason-other = Outra coisa
report-note = Algo a acrescentar
report-note-placeholder = Opcional
report-also-block = Bloquear também
report-also-block-hint = Deixar de ver as mensagens e de ouvir essa pessoa.
report-send = Enviar denúncia
report-sent = Denúncia enviada aos moderadores.
report-refused-no-message = Essa mensagem não está mais no servidor.
report-refused-no-user = Essa conta não está mais no servidor.
report-refused-yourself = Você não pode denunciar a si mesmo.
report-refused-too-many = Denúncias demais em pouco tempo. Tente de novo em alguns minutos.
blocked-header = Pessoas bloqueadas
blocked-not-connected = Conecte-se a um servidor para ver quem você bloqueou lá.
blocked-lead = Você não vê as mensagens nem ouve essas pessoas, e elas não podem lhe enviar mensagens diretas. Elas não são avisadas.
blocked-none = Ninguém
unblock = Desbloquear
account-delete-header = Excluir sua conta
account-delete-lead = Sua conta em { $server }.
account-delete-note = Remove sua conta neste servidor e tudo o que você publicou nele: mensagens, arquivos, mensagens diretas. Não dá para desfazer. Para entrar de novo você vai precisar de um convite novo.
account-delete = Excluir minha conta
account-delete-confirm = Excluir tudo — tem certeza?
account-deleting = Excluindo…
account-delete-last-admin = Você é o único admin deste servidor. Torne outra pessoa admin primeiro e depois exclua sua conta.
account-deleted = Sua conta em { $server } foi excluída, com tudo o que você publicou nele. Para entrar de novo você vai precisar de um convite novo.
admin-reports = Denúncias
admin-reports-lead = O que as pessoas denunciaram aos moderadores, mais recentes primeiro.
admin-reports-show-resolved = Mostrar denúncias tratadas
admin-reports-none = Nenhuma denúncia aberta.
admin-reports-by = Denunciado por
admin-reports-by-gone = Denunciado por uma conta excluída
admin-reports-resolve = Marcar como tratada
admin-reports-delete-message = Excluir mensagem
admin-reports-what-general = uma mensagem no General
admin-reports-what-dm = uma mensagem direta
admin-reports-what-person = a pessoa
admin-act-report-message = denunciou uma mensagem de
admin-act-report-user = denunciou
admin-act-report-resolve = tratou a denúncia
admin-act-user-delete = excluiu, a pedido do dono, a conta
chat-admin-reports-header = { $count } denúncias, mais recentes primeiro
# After a report /reports lists that was already dealt with.
chat-admin-report-handled = tratada
chat-command-usage-resolve = Uso: /resolve <número da denúncia> (veja /reports)
# ── A linha de termos do cartão de conexão (1.2) ─────────────────────
terms-notice = Ao conectar, você concorda com os
terms-link = termos e condições

# ── Atividade do Geral (1.2): o botão do cabeçalho, o vazio ──────────
activity-show = Atividade
activity-hide = Voltar às mensagens
activity-empty-title = Nenhuma atividade ainda
activity-empty = Entradas, saídas, mudanças de sala e telas compartilhadas aparecem aqui enquanto você está conectado.

# ── Configurações → Licenças de código aberto (1.2) ────────────────────
tab-licenses = Licenças
licenses-title = Licenças de código aberto
licenses-lead = baucord é software livre sob a Apache License 2.0, feito sobre o software de código aberto abaixo. Cada parte mantém sua própria licença; abra uma para lê-la.
licenses-section-components = Componentes
licenses-section-crates = Crates do Rust ({ $count })
licenses-crates-count = { $count ->
    [one] 1 crate
   *[other] { $count } crates
}
licenses-used-by = Usado por: { $crates }

# ── Intervalos de tempo, e o que o servidor recusou (review P3-30) ────
span-days = { $n } d
span-hours = { $n } h
span-minutes = { $n } min
span-seconds = { $n } s
span-days-hours = { $d } d { $h } h
span-hours-minutes = { $h } h { $m } min
span-ago = há { $span }
share-denied = Não foi possível compartilhar a tela: { $why }
watch-denied = Não é possível assistir { $name }: { $why }
deny-share-limit = o servidor atingiu o limite de telas compartilhadas
deny-viewer-limit = esta transmissão atingiu o limite de espectadores
deny-not-sharing = esse usuário não está compartilhando a tela
deny-dimensions = o servidor recusou o tamanho da transmissão
deny-not-in-channel = esta transmissão só aparece no canal de voz dela
deny-not-permitted = você não tem permissão
channel-deny-missing = Esse canal não existe mais.
channel-deny-not-permitted = Você não tem permissão para entrar nesse canal.
upload-deny-too-large = o arquivo é maior do que este servidor aceita
upload-deny-bad-type = só são aceitas imagens PNG, JPEG e WebP
upload-deny-damaged = o arquivo chegou corrompido
upload-deny-refused = o servidor recusou o arquivo
upload-deny-quota = você já usou sua cota de envios de hoje
upload-deny-full = o servidor não tem mais espaço para arquivos
upload-deny-not-permitted = você não tem permissão para enviar arquivos aqui
channel-flag-afk = AFK
channel-flag-temporary = temporário
channel-flag-password = com senha
channel-flag-permanent = permanente
channel-list-limit = limite { $limit }
admin-list-no-roles = sem cargos
admin-list-key = chave { $id }
admin-list-uses = usos { $uses }
admin-error-no-permission = você não tem permissão para isso
admin-error-no-user = esse usuário não existe
admin-error-no-restriction = essa restrição não existe
admin-error-no-invite = esse convite não existe
admin-error-no-role = esse cargo não existe
admin-error-no-channel = esse canal não existe
admin-error-outranked = a pessoa tem o seu nível ou um maior
admin-error-last-admin = o último administrador continua administrador
admin-error-yourself = não em você mesmo
