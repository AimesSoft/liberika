// Copy into a temporary Flutter iOS app with erika_flutter as a path dependency
// and assets/test.mp4 declared in its pubspec. See docs/ios-dfm-frame-pacing.md.
import 'dart:async';
import 'dart:convert';
import 'dart:developer';
import 'dart:io';

import 'package:erika_flutter/erika_flutter.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter/services.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  if (!Platform.isIOS) throw UnsupportedError('This smoke test is iOS only.');
  await SystemChrome.setPreferredOrientations([
    DeviceOrientation.landscapeLeft,
    DeviceOrientation.landscapeRight,
  ]);
  runApp(WidgetsApp(
    color: const Color(0xff000000),
    builder: (context, child) => const _Smoke(),
  ));
}

class _Smoke extends StatefulWidget {
  const _Smoke();
  @override
  State<_Smoke> createState() => _SmokeState();
}

class _SmokeState extends State<_Smoke> {
  final player = ErikaPlayer(hdrDebug: true, allowBackgroundPlayback: true);
  final errors = <String>[];
  StreamSubscription<ErikaPlayerEvent>? subscription;
  ErikaPlayerEvent? latest;
  bool ready = false;
  bool playing = false;

  @override
  void initState() {
    super.initState();
    subscription = player.events.listen((event) {
      latest = event;
      if (event.error != null) errors.add(event.error!);
    });
    registerExtension('ext.erikaSmoke.control', (_, params) async {
      try {
        if (params['playing'] == 'false' && playing) {
          await player.pause();
          playing = false;
        }
        if (params.containsKey('position')) {
          await player.seek(_duration(params['position']!));
        }
        if (params.containsKey('rate')) {
          await player.setPlaybackRate(double.parse(params['rate']!));
        }
        if (params.containsKey('offset')) {
          await player.setDanmakuGlobalOffset(_duration(params['offset']!));
        }
        if (params.containsKey('visible')) {
          await player.setDanmakuEnabled(params['visible'] == 'true');
        }
        if (params['playing'] == 'true' && !playing) {
          await player.play();
          playing = true;
        }
        return ServiceExtensionResponse.result(jsonEncode(await snapshot()));
      } catch (error) {
        return ServiceExtensionResponse.error(
          ServiceExtensionResponse.extensionError,
          error.toString(),
        );
      }
    });
    registerExtension('ext.erikaSmoke.snapshot', (_, params) async {
      return ServiceExtensionResponse.result(jsonEncode(await snapshot()));
    });
    unawaited(prepare());
  }

  Duration _duration(String seconds) => Duration(
        microseconds:
            (double.parse(seconds) * Duration.microsecondsPerSecond).round(),
      );

  Future<Map<String, Object?>> snapshot() async => {
        'ready': ready,
        'position': (latest?.position.inMicroseconds ?? 0) / 1000000,
        'state': latest?.state.name,
        'errors': errors,
        'stats': await const MethodChannel('erika_flutter/player').invokeMethod(
          'getPresenterStats',
          {'playerId': await player.ensureCreated()},
        ),
      };

  Future<void> prepare() async {
    try {
      final data = await rootBundle.load('assets/test.mp4');
      final file = File('${Directory.systemTemp.path}/erika-dfm-smoke.mp4');
      await file.writeAsBytes(data.buffer.asUint8List());
      await player.open(file.path);
      await player.setDanmakuConfig(
        fontSize: 25,
        displayArea: 1,
        scrollDurationSeconds: 9,
        mergeDuplicates: false,
      );
      await player.loadDanmakuJson(jsonEncode({
        'comments': List.generate(
            900,
            (i) => {
                  'id': i + 1,
                  'time': i / 30,
                  'content': 'DFM iOS ${i.toString().padLeft(3, '0')}',
                  'type': 'scroll',
                  'color': '#ffffff',
                }),
      }));
      await player.play();
      await Future<void>.delayed(const Duration(milliseconds: 500));
      await player.pause();
      await player.seek(const Duration(seconds: 8));
      ready = true;
      debugPrint('ERIKA_SMOKE_READY');
    } catch (error) {
      errors.add(error.toString());
      debugPrint('ERIKA_SMOKE_ERROR $error');
    }
  }

  @override
  Widget build(BuildContext context) => ColoredBox(
        color: const Color(0xff000000),
        child: ErikaVideoView(player: player),
      );

  @override
  void dispose() {
    subscription?.cancel();
    player.dispose();
    super.dispose();
  }
}
