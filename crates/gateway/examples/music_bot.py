#!/usr/bin/env python3
"""A music bot: `!play <url>` joins the asker's channel and plays the
URL (yt-dlp → ffmpeg → the gateway's voice sink); `!stop` stops.

Needs: `pip install websockets`, `yt-dlp` and `ffmpeg` on PATH, and a
baucord-gateway on 127.0.0.1:8765 whose bot has JOIN_CHANNELS and SPEAK
(a member does). The gateway paces the audio: this script pushes
ffmpeg's output as fast as the socket takes it, 100 ms at a time."""
import json
import os
import subprocess
import threading

from websockets.sync.client import connect

GATEWAY = os.environ.get("BAUCORD_GATEWAY", "ws://127.0.0.1:8765/")
PIECE = 4800 * 4  # 100 ms of f32 mono at 48 kHz


class Player:
    """One ffmpeg at a time, its stdout streamed to the gateway."""

    def __init__(self, ws):
        self.ws = ws
        self.proc = None
        self.lock = threading.Lock()

    def play(self, url):
        self.stop()
        # yt-dlp | ffmpeg, piped here rather than by a shell: the same on
        # Windows, where cmd.exe took shlex's quotes literally and cut a
        # URL at its `&`. `--` keeps a "URL" from being read as an option.
        ytdlp = subprocess.Popen(["yt-dlp", "-q", "-o", "-", "--", url], stdout=subprocess.PIPE)
        proc = subprocess.Popen(
            ["ffmpeg", "-loglevel", "error", "-i", "-", "-f", "f32le", "-ar", "48000", "-ac", "1", "-"],
            stdin=ytdlp.stdout,
            stdout=subprocess.PIPE,
        )
        ytdlp.stdout.close()  # ffmpeg's now; yt-dlp stops when ffmpeg does
        proc.upstream = ytdlp
        with self.lock:
            self.proc = proc
        threading.Thread(target=self._pump, args=(proc,), daemon=True).start()

    def _pump(self, proc):
        while True:
            piece = proc.stdout.read(PIECE)
            if not piece:
                break
            with self.lock:
                if self.proc is not proc:
                    break
            self.ws.send(piece)  # blocks when ten seconds are queued: the pacing
        proc.wait()
        proc.upstream.wait()
        # Ended by itself (not a !stop or the next !play) with yt-dlp
        # failing: say so, rather than going quiet.
        with self.lock:
            failed = self.proc is proc and proc.upstream.returncode != 0
            if self.proc is proc:
                self.proc = None
        if failed:
            action(self.ws, "CREATE_MESSAGE",
                   text="couldn't play that (yt-dlp failed; if every link fails, update it: yt-dlp -U)")

    def stop(self):
        with self.lock:
            proc, self.proc = self.proc, None
        if proc:
            proc.kill()
            proc.upstream.kill()
            self.ws.send(json.dumps({"op": "voice", "t": "STOP"}))


def action(ws, t, **d):
    ws.send(json.dumps({"op": "action", "t": t, "d": d}))


with connect(GATEWAY) as ws:
    hello = json.loads(ws.recv())
    channels = {}  # client_id -> channel_id, from READY and VOICE_STATE
    ready = hello["d"]["ready"]
    if ready:
        channels = {m["client_id"]: m["channel_id"] for m in ready["members"]}
    player = Player(ws)
    while True:
        f = json.loads(ws.recv())
        if f["op"] == "error":
            print("gateway says:", f["d"])
            continue
        if f["op"] != "dispatch":
            continue
        t, d = f["t"], f["d"]
        if t == "READY":
            channels = {m["client_id"]: m["channel_id"] for m in d["members"]}
        elif t in ("VOICE_STATE", "MEMBER_ADD"):
            channels[d["client_id"]] = d["channel_id"]
        elif t == "MEMBER_REMOVE":
            channels.pop(d["client_id"], None)
        elif t == "MESSAGE_CREATE" and not d["own"]:
            words = d["text"].split(maxsplit=1)
            if words[:1] == ["!play"] and len(words) == 2:
                url = words[1].strip()
                # Links only: yt-dlp guesses https:// in front of a bare
                # word and fetches whatever that names.
                if not url.lower().startswith(("http://", "https://")):
                    action(ws, "CREATE_MESSAGE", text="!play takes a link (https://...)")
                    continue
                channel = channels.get(d["client_id"])
                if channel is None:
                    action(ws, "CREATE_MESSAGE", text="join a voice channel first, %s" % d["name"])
                    continue
                action(ws, "JOIN_CHANNEL", channel_id=channel)
                action(ws, "CREATE_MESSAGE", text="playing %s for %s" % (url, d["name"]))
                player.play(url)
            elif words[:1] == ["!stop"]:
                player.stop()
                action(ws, "CREATE_MESSAGE", text="stopped")
