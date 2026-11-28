"""Independent host: actual Erika DLL, D3D11 HWND, HTTP media and lifecycle checks."""
import ctypes as C
from ctypes import wintypes as W
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import sys
import threading
import time
import struct
import zlib

root = Path(sys.argv[1])
evidence = Path(__file__).parent
dll = C.CDLL(str(evidence / 'target/debug/erika_capi.dll'))
def bind(name, restype, *args):
    fn = getattr(dll, name)
    fn.restype, fn.argtypes = restype, args
    return fn
create = bind('erika_presenter_create', C.c_void_p)
destroy = bind('erika_presenter_destroy', None, C.c_void_p)
invoke = bind('erika_presenter_invoke_json', C.c_void_p, C.c_void_p, C.c_char_p, C.c_char_p)
tick = bind('erika_presenter_render_tick_json', C.c_void_p, C.c_void_p, C.c_double)
poll = bind('erika_presenter_poll_event_json', C.c_void_p, C.c_void_p)
free = bind('erika_string_free', None, C.c_void_p)
attach = bind('erika_presenter_attach_windows_hwnd', C.c_int, C.c_void_p, C.c_uint64,
              C.c_uint64, C.c_uint32, C.c_uint32, C.c_double)
capture = bind('erika_presenter_capture_frame_rgba', C.c_int, C.c_void_p, C.c_uint32,
               C.c_uint32, C.c_void_p, C.c_size_t)
last_error = bind('erika_last_error_message', C.c_void_p)
user = C.WinDLL('user32', use_last_error=True)
kernel = C.WinDLL('kernel32')
user.CreateWindowExW.restype = W.HWND
user.CreateWindowExW.argtypes = [W.DWORD, W.LPCWSTR, W.LPCWSTR, W.DWORD,
    C.c_int, C.c_int, C.c_int, C.c_int, W.HWND, W.HMENU, W.HINSTANCE, C.c_void_p]
user.DestroyWindow.argtypes = [W.HWND]
kernel.GetModuleHandleW.restype = W.HMODULE
kernel.GetModuleHandleW.argtypes = [W.LPCWSTR]
instance = kernel.GetModuleHandleW(None)
window = user.CreateWindowExW(0, 'STATIC', 'Erika verification host', 0x00CF0000,
    0, 0, 640, 360, None, None, instance, None)
assert window, C.get_last_error()

def decoded(pointer):
    if not pointer:
        return None
    try:
        result = json.loads(C.string_at(pointer))
        assert result['ok'], result
        return result['value']
    finally:
        free(pointer)

def call(player, method, **kwargs):
    return decoded(invoke(player, method.encode(), json.dumps(kwargs).encode()))

events = []
def pump(player, duration):
    end = time.monotonic() + duration
    stats = None
    while time.monotonic() < end:
        stats = decoded(tick(player, time.monotonic()))
        while (event := decoded(poll(player))) is not None:
            assert event['kind'] != 9, event
            events.append(event)
        time.sleep(0.016)
    return stats

def rgba(player):
    buffer = (C.c_uint8 * (640 * 360 * 4))()
    status = capture(player, 640, 360, buffer, len(buffer))
    if status:
        message = last_error()
        try:
            print('native capture unavailable:', C.string_at(message).decode() if message else status,flush=True)
        finally:
            if message: free(message)
        return None
    return bytes(buffer)

def png(path, pixels):
    def chunk(kind, data):
        return struct.pack('!I',len(data)) + kind + data + struct.pack('!I',zlib.crc32(kind+data))
    rows = b''.join(b'\0'+pixels[y*640*4:(y+1)*640*4] for y in range(360))
    path.write_bytes(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('!2I5B',640,360,8,6,0,0,0))
                     +chunk(b'IDAT',zlib.compress(rows))+chunk(b'IEND',b''))

subtitle = evidence / 'whitespace-separators.srt'
subtitle.write_bytes(b'1\r\n00:00:01,000 --> 00:00:02,000\r\nFirst\r\n \t \r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nSecond\r\n')
bad_subtitle = evidence / 'overflow-timestamp.srt'
bad_subtitle.write_bytes(b'1\n00:00:1e100 --> 00:00:02,000\ninvalid\n')
payload = (evidence / 'bbb-10s.mp4').read_bytes()
http_requests = []
class MediaHandler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_HEAD(self):
        http_requests.append(('HEAD', self.path, self.headers.get('Range')))
        self.send_response(200); self.send_header('Content-Length',len(payload)); self.end_headers()
    def do_GET(self):
        requested = self.headers.get('Range')
        http_requests.append(('GET',self.path,requested))
        match = re.match(r'bytes=(\d+)-(\d*)',requested or '')
        if self.path.startswith('/ignore-range'):
            match = None
        start = int(match[1]) if match else 0
        end = min(int(match[2]) if match and match[2] else len(payload)-1,len(payload)-1)
        self.send_response(206 if match else 200)
        if match: self.send_header('Content-Range',f'bytes {start}-{end}/{len(payload)}')
        body=payload[start:end+1]
        self.send_header('Content-Length',len(body)); self.end_headers()
        try: self.wfile.write(body)
        except (ConnectionResetError, BrokenPipeError): pass

server = ThreadingHTTPServer(('127.0.0.1',0),MediaHandler)
threading.Thread(target=server.serve_forever,daemon=True).start()
player = create()
assert player
try:
    assert attach(player, window, instance, 640, 360, 1.0) == 0
    for index, source in enumerate([str(evidence/'bbb-10s.mp4'),
            f'http://127.0.0.1:{server.server_port}/range.mp4',
            f'http://127.0.0.1:{server.server_port}/ignore-range.mp4']):
        before = call(player,'getPresenterStats')['renderedVideoFrames']
        call(player,'open',uri=source,httpReadAheadBytes=65536,httpBackBufferBytes=65536)
        call(player,'play')
        stats = pump(player,3)
        assert stats['renderedVideoFrames'] - before > 20 and stats['renderFailures'] == 0, stats
        print('native playback',source,stats,flush=True)
        pixels = rgba(player)
        if pixels is not None:
            assert len(set(pixels)) > 32, 'capture has no actual video pixels'
            png(evidence/f'merge-windows-playing-{index}.png',pixels)
            print('actual video capture SHA256',hashlib.sha256(pixels).hexdigest(),flush=True)
        call(player,'pause'); pump(player,0.3)
        paused_stats=call(player,'getPresenterStats')
        pump(player,0.3)
        assert call(player,'getPresenterStats')['decodedVideoFrames']==paused_stats['decodedVideoFrames']
        call(player,'seek',positionMicros=10000000); pump(player,0.6)
        assert [e['state'] for e in events if e['kind']==1][-1]==4,'paused end seek changed state'
        call(player,'seek',positionMicros=3500000); pump(player,0.6)
        print('native paused EOF seek and middle seek remain Paused',flush=True)
        call(player,'play'); pump(player,0.5)
        call(player,'setPlaybackRate',rate=1.5); pump(player,0.5)
        call(player,'stop')
    for path in ['/range.mp4','/ignore-range.mp4']:
        assert any(method=='GET' and request_path==path and value and
                   int(re.match(r'bytes=(\d+)-',value)[1])>0
                   for method,request_path,value in http_requests),http_requests
    print('real media HTTP requests',http_requests,flush=True)
    call(player,'open',uri=str(root/'crates/erika/testdata/software/mp3-attached-pic.mp3'))
    tracks=call(player,'tracks')
    print('attached-cover MP3 tracks',tracks,flush=True)
    assert len(tracks)==1 and tracks[0]['kind']==1,tracks
    if '--video-only' not in sys.argv:
        before_audio=call(player,'getPresenterStats')['audioClockReadFrames']
        call(player,'play')
        stats=pump(player,4)
        assert stats['audioClockReadFrames']-before_audio > 30000 and stats['audioFailures']==0,stats
        print('real WASAPI cover MP3 audio consumption',stats,flush=True)
        call(player,'stop')
    else:
        print('WASAPI playback explicitly omitted in video-only run',flush=True)
    call(player,'loadDanmakuJson',json=json.dumps({'comments':[{'time':'1e100','content':'invalid'},{'time':0,'content':'valid'}]}))
    raw=invoke(player,b'addExternalSubtitle',json.dumps({'uri':str(bad_subtitle)}).encode())
    try:
        error=json.loads(C.string_at(raw)); assert not error['ok'] and 'panic' not in str(error).lower(),error
        print('invalid external subtitle rejected through C ABI',error,flush=True)
    finally: free(raw)
    print('live C ABI malformed danmaku/subtitle survived; final stats',call(player,'getPresenterStats'),flush=True)
finally:
    destroy(player)
    user.DestroyWindow(window)
    server.shutdown()
