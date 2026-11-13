"""The site's screenshots (web/img), rendered from the real UI at 2x.

One fixture per shot — the same server and people throughout — loaded
into `crates/ui/slint/main.slint` with slint-viewer (SESSION-GUIDE.md §1
and §6). `share.png` is the stream viewer's window, rendered with a drawn
frame, composited over the call (a stream cannot play headless); the
camera tiles show drawn frames too (`cam-*.png`).

    SLINT_VIEWER=/path/to/slint-viewer python3 tools/site-shots/shots.py [shot …]

writes PNGs (and the fixtures) to tools/site-shots/out; copy the ones
you want into web/img. SCALE=1 renders at 1x for a quick look.
"""
import json, re, subprocess, sys, os, glob

ASSETS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(ASSETS, '..', '..', 'crates')
SL = ROOT + '/ui/slint'
HERE = os.path.join(ASSETS, 'out')
os.makedirs(HERE, exist_ok=True)
BLANK = HERE + '/blank.png'
VIEWER = os.environ.get('SLINT_VIEWER', 'slint-viewer')
LUC = glob.glob(os.path.expanduser('~/.cargo/registry/src/*/lucide-slint-*/lucide.slint'))[-1]
if not os.path.exists(BLANK):
    from PIL import Image
    Image.new('RGBA', (1, 1)).save(BLANK)

src = ''.join(open(f).read() for f in [SL + '/globals.slint', SL + '/admin.slint', SL + '/settings.slint'])


def struct(name):
    m = re.search(r'export struct ' + name + r'\s*\{([^{}]*)\}', src, re.S)
    body = re.sub(r'//[^\n]*', '', m.group(1))
    fields = {}
    for part in re.split(r'[,\n]', body):
        part = part.strip()
        if ':' in part:
            k, t = [x.strip() for x in part.split(':', 1)]
            fields[k] = t
    return fields


def default(t):
    return {'string': '', 'int': 0, 'bool': False, 'color': '#00000000', 'image': BLANK,
            'length': 0, 'float': 0}.get(t)


def row(sname, **kw):
    r = {k: default(t) for k, t in struct(sname).items() if default(t) is not None}
    for k, v in kw.items():
        r[k.replace('_', '-')] = v
    return r


def tr():
    names = re.findall(r'in property <string> ([\w-]+);', open(SL + '/tr.slint').read())
    ftl = {}
    for line in open(ROOT + '/i18n/locales/en.ftl'):
        m = re.match(r'^([\w-]+) = (.*)$', line.rstrip('\n'))
        if m:
            ftl[m.group(1)] = m.group(2)
    out = {n: ftl.get(n, n) for n in names}
    out['rec-label'] = 'REC'
    out['audio-output-hint'] = ''   # set by hand on Android only
    return out


# ── The people (as the site's earlier shots had them) ──────────────
TINT = {'empe': '#5B3A8E', 'maru': '#3D6B4E', 'tomi': '#8E5A3A', 'lucas': '#3A5A8E', 'vicky': '#8E3A5A',
        'nico': '#5A6B3D'}
NAMECOL = {'maru': '#B5E07A', 'vicky': '#F28C38', 'empe': None, 'tomi': None, 'lucas': None}


def user(name, ch, cid, **kw):
    base = dict(name=name, channel_id=ch, initial=name[0].upper(), tile_tint=TINT[name], client_id_lo=cid,
                volume_pct=100, xp_pct=-1)
    if NAMECOL.get(name):
        base.update(name_color=NAMECOL[name], has_name_color=True)
    base.update(kw)
    return row('UserRow', **base)


def chan(name, cid, label, **kw):
    return row('UserRow', is_channel=True, channel_id=cid, name=name, member_label=label, volume_pct=100,
               xp_pct=-1, **kw)


def roster(in_lobby_call=True, recording=False):
    return [
        chan('Lobby', 1, '4', is_our_channel=in_lobby_call, timer_label='01:12:40', recording=recording,
             rec_tip='Recording, started by empe · 00:18:22'),
        user('empe', 1, 1, is_us=True, role=4),
        user('maru', 1, 2, self_muted=False, speaking=True),
        user('tomi', 1, 3, role=1),
        user('vicky', 1, 5),
        chan('Gaming', 2, '1/8', locked=True, timer_label='00:41:05'),
        user('lucas', 2, 4, role=3, sharing=True),
        chan('AFK', 3, '0'),
    ]


def chat(user_, body, ts, own=False, cont=False, **kw):
    r = dict(kind=0, user=user_, body=body, ts=ts, own=own, continuation=cont, initial=user_[0].upper(),
             tile_tint=TINT.get(user_, '#444444'))
    if NAMECOL.get(user_):
        r.update(name_color=NAMECOL[user_], has_name_color=True)
    r.update(kw)
    return row('ChatRow', **r)


GENERAL = [
    row('ChatRow', kind=4, body='Today'),
    row('ChatRow', kind=1, body='maru joined'),
    chat('maru', 'movie night? lucas is already sharing in Gaming', '21:03'),
    chat('empe', 'give me 5, grabbing snacks', '21:04', own=True),
    row('ChatRow', kind=1, body='tomi joined'),
    chat('tomi', 'someone send the trailer link', '21:05'),
    chat('maru', 'https://youtu.be/dQw4w9WgXcQ', '21:05'),
    chat('empe', 'ok moving to Gaming, bring headphones', '21:06', own=True),
    chat('empe', 'bringing the good ones this time', '21:06', own=True, cont=True),
    chat('vicky', "hurry up, it's starting", '21:07'),
]


def dms(active=-1):
    rows = [
        row('DmRow', name='maru', online=True, initial='M', tile_tint=TINT['maru'],
            preview='snacks list, add yours', when='21:01', status='In Lobby', shown=True, unread=2),
        row('DmRow', name='Server', admin=True, initial='S', tile_tint='#444444',
            preview='Lobby_2026-11-12_1h12m40s.wav', when='20:48', shown=True),
        row('DmRow', name='lucas', online=True, initial='L', tile_tint=TINT['lucas'],
            preview='You: trailer + subs for tonight', when='20:58', status='In Gaming', shown=True),
        row('DmRow', name='tomi', online=True, initial='T', tile_tint=TINT['tomi'],
            preview='see you friday', when='Tue', status='In Lobby', shown=True),
        row('DmRow', name='nico', initial='N', tile_tint=TINT['nico'], preview='gg', when='Nov 3', shown=True),
    ]
    if active >= 0:
        rows[active]['active'] = True
    return rows


def tiles():
    return [
        row('StageTile', name='maru', index=2, initial='M', tile_tint=TINT['maru'], client_id_lo=2, speaking=True,
            camera=True, frame=ASSETS + '/cam-maru.png', level_short='Lvl 12', xp_pct=60,
            name_color=NAMECOL['maru'], has_name_color=True),
        row('StageTile', name='tomi', index=3, initial='T', tile_tint=TINT['tomi'], client_id_lo=3, role=1,
            level_short='Lvl 3', xp_pct=20),
        row('StageTile', name='vicky', index=4, initial='V', tile_tint=TINT['vicky'], client_id_lo=5,
            self_muted=True, level_short='Lvl 7', xp_pct=45, name_color=NAMECOL['vicky'], has_name_color=True),
        row('StageTile', name='empe', index=1, initial='E', tile_tint=TINT['empe'], client_id_lo=1, is_us=True,
            role=4, camera=True, frame=ASSETS + '/cam-empe.png', level_short='Lvl 21', xp_pct=85),
    ]


def base_state():
    return {
        'phase': 'main', 'connected': True, 'in-lobby': False, 'in-call': True, 'server-name': 'Los Conos',
        'greeting': 'Movie night every Friday', 'users-count-label': 'Users: 6',
        'users': roster(), 'chat': GENERAL, 'dms': dms(), 'stage-tiles': tiles(),
        'stage-name': 'Lobby', 'stage-timer': '01:12:40', 'stage-channel': 1,
        'view-main': 1, 'view-chat': True, 'view-messages': False,
        'general-unread': 0, 'dm-unread-total': 2, 'perm-panel': True, 'perm-create-channels': True,
        'perm-create-invites': True, 'camera-available': True, 'active-dm': -1,
        'status-text': 'Connected', 'latency-line': 'Audio: 12ms | Net: 35ms',
        'latency-up': '0.1', 'latency-down': '0.4', 'resource-line': 'CPU 3.1% · Mem 44 MB',
        'show-timestamps': True, 'max-chars': 400, 'perm-dm': True,
    }


def file_row(user_, ts, name, kind, meta, own=False, saved=False, transfer=0, pct=0, cont=False):
    return chat(user_, name, ts, own=own, cont=cont, kind=5, file_kind=kind, file_meta=meta, file_saved=saved,
                transfer_kind=transfer, transfer_pct=pct)


def shots():
    out = {}
    # Hero: in a call, the channel list and the chat.
    s = base_state()
    out['call'] = ({'State': s}, '1000x640')

    # Channel view: cameras and tiles, the chat hidden.
    s = base_state()
    s.update({'view-main': 2, 'view-chat': False, 'stage-recording': True, 'stage-can-stop': True,
              'stage-rec-tip': 'Recording, started by empe · 00:18:22', 'general-unread': 3,
              'users': roster(recording=True)})
    out['stage'] = ({'State': s}, '1000x640')

    # Files, pictures and direct messages: Messages with maru.
    s = base_state()
    s.update({'view-messages': True, 'active-dm': 0, 'conv-name': 'maru', 'dms': dms(0),
              'chat': [
                  row('ChatRow', kind=4, body='Today'),
                  chat('maru', 'trailer + subs for tonight', '20:58'),
                  file_row('maru', '20:58', 'trailer-1080p.mp4', 'MP4', '48.3 MB', saved=True, cont=True),
                  file_row('maru', '20:58', 'subtitles-es.srt', 'SRT', '62 KB', cont=True),
                  chat('empe', 'perfect, thanks', '20:59', own=True),
                  chat('maru', 'snacks list, add yours', '21:01'),
                  file_row('maru', '21:01', 'snacks.xlsx', 'XLSX', '18 KB', cont=True),
                  file_row('empe', '21:03', 'poster.png', 'PNG', 'Uploading 63%', own=True, transfer=1, pct=63),
              ]})
    s['dm-unread-total'] = 0
    s['dms'][0]['unread'] = 0
    out['files'] = ({'State': s}, '1000x640')

    # The user menu on maru.
    s = base_state()
    M = lambda label, id_, **kw: row('MenuEntry', **{'label': label, 'id': id_, 'enabled': True, 'submenu': -1, **kw})
    s.update({'menu-open': True, 'menu-x': 170, 'menu-y': 132, 'menu-volume': 1.25,
              'menu-entries': [M('', 1, slider=True), M('Mute for me', 2), M('Message', 3),
                               M('Move to', 4, separator=True, submenu=0), M('Role', 5, submenu=0),
                               M('Mute', 6, submenu=0), M('Disconnect', 7), M('Kick', 8, danger=True),
                               M('Ban', 9, danger=True)],
              'menu-submenus': [[M('Gaming', 20), M('AFK', 21)]]})
    out['popover'] = ({'State': s}, '1000x640')

    # Joining a locked channel.
    s = base_state()
    s.update({'channel-prompt-open': True, 'channel-prompt-name': 'Gaming', 'in-call': False, 'in-lobby': True,
              'stage-name': '', 'stage-timer': '', 'users': roster(in_lobby_call=False)})
    s['users'] = [u for u in s['users'] if u['name'] != 'empe']
    s['users'][0]['member-label'] = '3'
    out['prompt'] = ({'State': s}, '1000x640')

    # Connect screen.
    s = {'phase': 'connect', 'nickname': 'empe', 'server-names': ['Los Conos', 'Movie night', 'Local test'],
         'server-index': 0}
    out['connect'] = ({'State': s}, '1000x640')

    # Settings: servers and audio.
    servers = [row('ServerRow', name='Los Conos', address='216.238.121.18:5000', is_default=True),
               row('ServerRow', name='Movie night', address='movies.example.net:5000'),
               row('ServerRow', name='Local test', address='127.0.0.1:5000')]
    s = base_state(); s.update({'settings-open': True, 'settings-tab': 5})
    out['settings-servers'] = ({'State': s, 'SettingsState': {
        'servers': servers, 'server-index': 0, 'server-name': 'Los Conos', 'server-ip': '216.238.121.18',
        'server-port': '5000', 'server-fingerprint': 'SHA256:9f4d…a8f9', 'connected': True}}, '760x660')
    s = base_state(); s.update({'settings-open': True, 'settings-tab': 2})
    out['settings-audio'] = ({'State': s, 'SettingsState': {
        'input-devices': ['System default', 'Microphone (BY-V20)'], 'input-index': 1,
        'output-devices': ['System default', 'Headphones (Qudelix-5K)'], 'output-index': 1,
        'transmission-modes': ['Voice activity', 'Push to talk'], 'transmission-index': 0,
        'mic-level': 0.42, 'gate-threshold': 0.12, 'input-gain': 1.0, 'output-gain': 1.0,
        'aec-modes': ['Off', 'On'], 'aec-index': 0,
        'sensitivity-line': 'Speak normally — the bar should turn green while you talk.'}}, '760x660')

    # The admin panel's Users tab.
    A = lambda name, roles, online, seen, idx, keys=1: row('AdminUserRow', user_id=name, name=name, roles=roles,
                                                            online=online, last_seen=seen, keys=keys,
                                                            role_index=idx)
    s = base_state(); s.update({'admin-open': True, 'admin-tab': 0})
    out['admin'] = ({'State': s, 'AdminState': {
        'users': [A('empe', 'admin', True, 'now', 3), A('lucas', 'organizer, streamer', True, 'now', 4),
                  A('maru', 'member', True, 'now', 1), A('vicky', 'member', True, 'now', 1),
                  A('tomi', 'guest', False, '2 h ago', 0), A('nico', 'member', False, '3 d ago', 1)],
        'role-names': ['guest', 'member', 'organizer', 'admin', 'streamer']}}, '820x660')
    return out


def render(name, data, size, scale=2, component='MainWindow'):
    data = dict(data)
    data['Tr'] = tr()
    path = f'{HERE}/{name}.json'
    json.dump(data, open(path, 'w'))
    env = dict(os.environ, SLINT_SCALE_FACTOR=str(scale))
    r = subprocess.run([VIEWER, SL + '/main.slint', '-L', 'lucide=' + LUC, '--component', component,
                        '--load-data', path, '--size', size, '--screenshot', f'{HERE}/{name}.png'],
                       env=env, capture_output=True, text=True, timeout=180)
    noise = [l for l in r.stderr.splitlines() if 'unknown property' not in l and l.strip()]
    if noise:
        print(name, '\n'.join(noise[:8]))


def share(scale):
    """The viewer window over the call: its own chrome, rendered with a
    drawn frame (a stream cannot play headless), composited with a
    shadow, as the earlier share.png was made."""
    from PIL import Image, ImageFilter
    render('viewer', {'peer-name': 'lucas', 'frame': ASSETS + '/share-scene.png',
                      'stats': '1080p · 60 fps · 4.2 Mb/s'}, '640x400', scale, 'StreamViewerWindow')
    base = Image.open(f'{HERE}/call.png').convert('RGBA')
    win = Image.open(f'{HERE}/viewer.png').convert('RGBA')
    k = scale
    x, y = int(340 * k), int(140 * k)
    shadow = Image.new('RGBA', base.size, (0, 0, 0, 0))
    sh = Image.new('RGBA', (win.width + int(40 * k), win.height + int(40 * k)), (0, 0, 0, 0))
    sh.paste((0, 0, 0, 150), (int(20 * k), int(20 * k), int(20 * k) + win.width, int(20 * k) + win.height))
    sh = sh.filter(ImageFilter.GaussianBlur(14 * k))
    shadow.paste(sh, (x - int(20 * k), y - int(12 * k)), sh)
    out = Image.alpha_composite(base, shadow)
    out.paste(win, (x, y))
    from PIL import ImageDraw
    ImageDraw.Draw(out).rectangle([x - 1, y - 1, x + win.width, y + win.height], outline=(58, 61, 66, 255),
                                  width=max(1, int(k)))
    out.convert('RGB').save(f'{HERE}/share.png')


if __name__ == '__main__':
    want = sys.argv[1:]
    scale = float(os.environ.get('SCALE', 2))
    for name, (data, size) in shots().items():
        if not want or name in want:
            render(name, data, size, scale=scale)
            print('rendered', name)
    if not want or 'share' in want:
        share(scale)
        print('rendered share')
