"""Independent Xlib/C ABI host verifies actual subtitle pixels and MP3 audio."""
import ctypes as C
import hashlib
import json
from pathlib import Path
import struct
import sys
import time
import zlib

evidence = Path('/review-logs')
dll = C.CDLL('/target/debug/liberika_capi.so')
def bind(name, result, *args):
    fn = getattr(dll, name)
    fn.restype, fn.argtypes = result, args
    return fn
create = bind('erika_presenter_create', C.c_void_p)
destroy = bind('erika_presenter_destroy', None, C.c_void_p)
invoke = bind('erika_presenter_invoke_json', C.c_void_p, C.c_void_p, C.c_char_p, C.c_char_p)
tick = bind('erika_presenter_render_tick_json', C.c_void_p, C.c_void_p, C.c_double)
poll = bind('erika_presenter_poll_event_json', C.c_void_p, C.c_void_p)
free = bind('erika_string_free', None, C.c_void_p)
attach = bind('erika_presenter_attach_wgpu_surface', C.c_int, C.c_void_p, C.c_int,
              C.c_uint64, C.c_uint64, C.c_uint32, C.c_uint32, C.c_double)
capture = bind('erika_presenter_capture_frame_rgba', C.c_int, C.c_void_p, C.c_uint32,
               C.c_uint32, C.c_void_p, C.c_size_t)
error = bind('erika_last_error_message', C.c_void_p)
xlib = C.CDLL('libX11.so.6')
xlib.XInitThreads()
xlib.XOpenDisplay.restype = C.c_void_p
xlib.XOpenDisplay.argtypes = [C.c_char_p]
xlib.XDefaultRootWindow.restype = C.c_ulong
xlib.XDefaultRootWindow.argtypes = [C.c_void_p]
xlib.XCreateSimpleWindow.restype = C.c_ulong
xlib.XCreateSimpleWindow.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int,
                                    C.c_uint, C.c_uint, C.c_uint, C.c_ulong, C.c_ulong]
xlib.XMapWindow.argtypes = [C.c_void_p, C.c_ulong]
xlib.XSync.argtypes = [C.c_void_p, C.c_int]
xlib.XDestroyWindow.argtypes = [C.c_void_p, C.c_ulong]
xlib.XCloseDisplay.argtypes = [C.c_void_p]
display = xlib.XOpenDisplay(None)
assert display
window = xlib.XCreateSimpleWindow(display, xlib.XDefaultRootWindow(display), 0, 0, 640, 360, 0, 0, 0)
xlib.XMapWindow(display, window)
xlib.XSync(display, 0)

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
        message = error()
        try:
            raise AssertionError(C.string_at(message).decode() if message else status)
        finally:
            if message:
                free(message)
    return bytes(buffer)
def png(path, image):
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
    filtered = b''.join(b'\0' + image[y*640*4:(y+1)*640*4] for y in range(360))
    path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B',640,360,8,6,0,0,0))
                     + chunk(b'IDAT',zlib.compress(filtered)) + chunk(b'IEND',b''))

player = create()
assert player
try:
    assert attach(player, 5, window, display, 640, 360, 1.0) == 0
    call(player, 'open', uri=str(evidence/'bbb-10s.mp4'))
    call(player, 'play')
    pump(player, 0.5)
    call(player, 'pause')
    pump(player, 0.2)
    subtitle_path = evidence / (sys.argv[1] if len(sys.argv)>1 else 'whitespace-separators.srt')
    track_id = call(player, 'addExternalSubtitle', uri=str(subtitle_path))
    print('selected subtitle:', subtitle_path, call(player, 'tracks'), flush=True)
    call(player, 'selectSubtitleTrack', trackId=track_id)
    call(player, 'seek', positionMicros=3500000)
    call(player, 'play')
    pump(player, 0.2)
    call(player, 'pause')
    stats = pump(player, 0.3)
    print('cue snapshot stats:', stats, flush=True)
    assert [e['state'] for e in events if e['kind']==1][-1] == 4
    with_subtitle = rgba(player)
    png(evidence/(subtitle_path.stem+'-second.png'), with_subtitle)
    call(player, 'selectSubtitleTrack', trackId=-1)
    pump(player, 0.2)
    without_subtitle = rgba(player)
    png(evidence/(subtitle_path.stem+'-disabled.png'), without_subtitle)
    changed = sum(with_subtitle[i:i+4] != without_subtitle[i:i+4] for i in range(0,len(with_subtitle),4))
    print('second cue at paused 3.5s:', changed, 'changed pixels', flush=True)
    assert changed > 100, changed
    print('subtitle pixel SHA256:', hashlib.sha256(with_subtitle).hexdigest(), flush=True)
    call(player, 'stop')
    destroy(player)
    player = create()
    assert player
    assert attach(player, 5, window, display, 640, 360, 1.0) == 0
    call(player, 'open', uri='/work/crates/erika/testdata/software/mp3-attached-pic.mp3')
    tracks = call(player, 'tracks')
    assert len(tracks)==1 and tracks[0]['kind']==1, tracks
    before = call(player, 'getPresenterStats')['audioClockReadFrames']
    call(player, 'play')
    stats = pump(player, 4.0)
    assert stats['audioFailures']==0, stats
    assert stats['audioClockReadFrames']-before >= 40000, stats
    print('cover MP3 track filtering passed:', tracks, flush=True)
    print('cover MP3 audio consumption observed:', stats['audioClockReadFrames']-before,
          'queued:', stats['audioClockQueuedFrames'], flush=True)
finally:
    destroy(player)
    xlib.XDestroyWindow(display, window)
    xlib.XCloseDisplay(display)
