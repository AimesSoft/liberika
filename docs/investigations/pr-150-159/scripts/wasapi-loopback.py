from pathlib import Path
import sys, time, wave
import numpy as np
import soundcard as sc
source = Path(__file__).with_name('windows-live-capi.py')
exec(compile(source.read_text().split("subtitle = evidence")[0], str(source), 'exec'))
player = create()
assert player
assert attach(player, window, instance, 640, 360, 1.0) == 0
speaker = sc.default_speaker()
mic = sc.get_microphone(speaker.id, include_loopback=True)
try:
    with mic.recorder(samplerate=48000, channels=2) as recording:
        call(player, 'open', uri=str(evidence/'cover-5s.mp3'))
        call(player, 'play')
        pump(player, 1)
        samples = recording.record(numframes=96000)
        stats = pump(player, 0.2)
    rms = float(np.sqrt(np.mean(samples**2)))
    peak = float(np.max(np.abs(samples)))
    assert rms > 0.001, (rms, peak, stats)
    assert stats['audioClockReadFrames'] > 48000 and stats['audioFailures'] == 0, stats
    pcm = (np.clip(samples, -1, 1) * 32767).astype('<i2')
    with wave.open(str(evidence/'merge-wasapi-loopback.wav'), 'wb') as output:
        output.setnchannels(2); output.setsampwidth(2); output.setframerate(48000)
        output.writeframes(pcm.tobytes())
    print('actual default WASAPI loopback RMS=',rms,'peak=',peak,'stats=',stats,flush=True)
finally:
    destroy(player)
    user.DestroyWindow(window)
