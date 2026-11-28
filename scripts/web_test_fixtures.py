#!/usr/bin/env python3
"""Generate small, redistributable browser playback fixtures with system FFmpeg."""
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
OUT=ROOT/'target/web/fixtures'
OUT.mkdir(parents=True,exist_ok=True)
def ffmpeg(*args):
    subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-y',*map(str,args)],check=True)
ffmpeg('-f','lavfi','-i','testsrc2=size=640x360:rate=24:duration=12','-f','lavfi','-i','sine=frequency=440:sample_rate=48000:duration=12','-i',ROOT/'crates/erika/testdata/web/subtitles.ass','-map','0:v','-map','1:a','-map','2:s','-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-g','48','-bf','2','-c:a','aac','-c:s','ass','-attach',ROOT/'crates/erika/assets/subfont.ttf','-metadata:s:t','mimetype=application/x-truetype-font',OUT/'test.mkv')
ffmpeg('-f','lavfi','-i','testsrc2=size=480x270:rate=24:duration=5','-f','lavfi','-i','sine=frequency=220:sample_rate=48000:duration=5','-c:v','libvpx-vp9','-deadline','realtime','-cpu-used','8','-c:a','libopus',OUT/'test.webm')
ffmpeg('-f','lavfi','-i','testsrc2=size=320x180:rate=24:duration=5','-f','lavfi','-i','sine=frequency=220:sample_rate=48000:duration=1','-c:v','libx264','-preset','ultrafast','-c:a','aac',OUT/'audio-tail.mp4')
print(OUT)
