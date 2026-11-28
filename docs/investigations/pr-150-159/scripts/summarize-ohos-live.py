import json
import argparse
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('root', nargs='?', type=Path, default=Path(__file__).resolve().parent)
parser.add_argument('--api', type=int, default=18)
parser.add_argument('--os', default='HarmonyOS 5.1.0.110(SP2DEVC00E110R4P11)')
args = parser.parse_args()
root = args.root
if not (root / "ohos-arkts-review.jsonl").exists():
    root = root.parent / "evidence"
summary = {"device": "127.0.0.1:5555", "os": args.os,
           "api": args.api, "abi": "x86_64", "bridges": {}}
stages = ("playing", "pause-settled", "paused", "paused-eof", "resumed", "surface-reconnected", "cover-audio", "recreated-playback")
for bridge in ("arkts", "flutter"):
    rows = [json.loads(line) for line in (root / f"ohos-{bridge}-review.jsonl").read_text().splitlines()]
    assert not any(row["stage"] == "FAILED" for row in rows), bridge
    assert any(row["stage"] == "COMPLETE" for row in rows), bridge
    stats = {row["stage"]: json.loads(row["value"])["value"] for row in rows if row["stage"] in stages}
    assert set(stats) == set(stages), bridge
    for data in stats.values():
        assert data["renderFailures"] == data["importFailures"] == data["audioFailures"] == 0
    assert stats["paused"]["decodedVideoFrames"] == stats["paused-eof"]["decodedVideoFrames"]
    assert stats["pause-settled"]["decodedVideoFrames"] == stats["paused"]["decodedVideoFrames"]
    assert stats["pause-settled"]["audioClockReadFrames"] == stats["paused"]["audioClockReadFrames"]
    assert any(row["stage"] == "paused-eof-state" and row["value"] == "4" for row in rows)
    assert stats["resumed"]["decodedVideoFrames"] > stats["paused"]["decodedVideoFrames"]
    assert stats["cover-audio"]["decodedVideoFrames"] == 0
    assert stats["cover-audio"]["audioClockReadFrames"] > 0
    assert stats["cover-audio"]["audioClockQueuedFrames"] == 0
    stale = json.loads(next(row["value"] for row in rows if row["stage"] == "stale-id"))
    assert stale["newId"] > stale["oldId"] and stale["newAlive"]
    assert not json.loads(stale["staleInvoke"])["ok"]
    assert stats["recreated-playback"]["renderedVideoFrames"] > 0
    assert stats["recreated-playback"]["audioClockReadFrames"] > 0
    summary["bridges"][bridge] = {"stats": stats, "stale_id": stale, "complete": True}
(root / "ohos-live-summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
print("Both production bridges passed real emulator playback, pause/EOF seek, surface reconnect, cover MP3 and stale-ID checks.")
