// Test-double smoke app: copy into a generated Flutter Linux runner.
// Uses ohos/tests/presenter_stub.cpp; this does not validate media decoding.
import 'dart:async';
import 'dart:io';
import 'package:flutter/widgets.dart';
import 'package:flutter/services.dart';

const player = MethodChannel('erika_flutter/player');
final shownTexture = ValueNotifier<int?>(null);
void main() {
  runApp(WidgetsApp(
    color: const Color(0xff000000),
    textStyle: const TextStyle(color: Color(0xff000000), fontSize: 14),
    builder: (context, child) => ColoredBox(
      color: const Color(0xffffffff),
      child: Center(
        child: ValueListenableBuilder<int?>(
          valueListenable: shownTexture,
          builder: (context, id, child) => id == null
            ? const Text('Erika owner-thread check')
            : SizedBox(width: 128, height: 72, child: Texture(textureId: id)),
        ),
      ),
    ),
  ));
  WidgetsBinding.instance.addPostFrameCallback((_) { smoke(); });
}
Future<void> smoke() async {
  try {
    final events = <dynamic>[];
    final subscription = const EventChannel('erika_flutter/events').receiveBroadcastStream().listen(events.add);
    final id = await player.invokeMethod<int>('create', {'outputMode': 0});
    final texture = await player.invokeMethod<int>('createTexture', {'width': 64, 'height': 64, 'scale': 1.0});
    await player.invokeMethod('attachView', {'playerId': id, 'viewId': texture});
    shownTexture.value = texture;
    await player.invokeMethod('open', {'playerId': id, 'uri': 'stub://owner-check'});
    await player.invokeMethod('play', {'playerId': id});
    await Future<void>.delayed(const Duration(milliseconds: 200));
    await player.invokeMethod('pause', {'playerId': id});
    await player.invokeMethod('seek', {'playerId': id, 'positionMicros': 1000});
    await player.invokeMethod('resizeTexture', {'textureId': texture, 'width': 128, 'height': 72, 'scale': 1.0});
    await player.invokeMethod('play', {'playerId': id});
    final bytes = await player.invokeMethod<Uint8List>('screenshot', {'playerId': id, 'width': 2, 'height': 3});
    if (bytes?.length != 24) throw StateError('capture size');
    final font = await player.invokeMethod<int>('registerSubtitleMemoryFont', {'playerId': id, 'data': Uint8List.fromList([1, 2, 3])});
    if (font != 42) throw StateError('font bytes');
    await player.invokeMethod('loadUpscalerModel', {'playerId': id, 'name': 'smoke', 'data': Uint8List.fromList([1, 2, 3])});
    await player.invokeMethod('clearUpscalerModel', {'playerId': id});
    await player.invokeMethod('detachView', {'playerId': id, 'viewId': texture});
    await player.invokeMethod('attachView', {'playerId': id, 'viewId': texture});
    if (Platform.environment['ERIKA_TEST_WAYLAND_OVERLAY'] == '1') {
      await player.invokeMethod('attachOverlay', {'playerId': id});
      for (var generation = 1; generation <= 2; generation++) {
        await player.invokeMethod('setOverlayFrame', {
          'playerId': id, 'generation': generation, 'visible': true,
          'x': 4.0, 'y': 4.0, 'width': 128.0 + generation, 'height': 72.0,
        });
        await Future<void>.delayed(const Duration(milliseconds: 80));
      }
      await player.invokeMethod('setOverlayFrame', {
        'playerId': id, 'generation': 2, 'visible': false,
      });
      await player.invokeMethod('detachOverlay', {'playerId': id, 'generation': 2});
      await player.invokeMethod('attachOverlay', {'playerId': id});
      await player.invokeMethod('setOverlayFrame', {
        'playerId': id, 'generation': 3, 'visible': true,
        'x': 4.0, 'y': 4.0, 'width': 128.0, 'height': 72.0,
      });
    }
    // Accepted method results still complete when dispose joins their owner.
    final slow = player.invokeMethod('slow', {'playerId': id});
    final disposing = player.invokeMethod('dispose', {'playerId': id});
    await slow; await disposing;
    await player.invokeMethod('releaseTexture', {'textureId': texture});
    await player.invokeMethod('dispose', {'playerId': id});
    if (events.isEmpty) throw StateError('no UI events');
    final atDispose = events.length;
    await Future<void>.delayed(const Duration(milliseconds: 80));
    if (events.length != atDispose) throw StateError('stale event after dispose');
    await subscription.cancel();
    stdout.writeln('LINUX_STUB_SMOKE passed open/play/pause/seek/resize/capture/font/model/detach/reattach/pending-result dispose; overlay=${Platform.environment['ERIKA_TEST_WAYLAND_OVERLAY']}; events=${events.length}');
    exit(0);
  } catch (error, stack) {
    stderr.writeln('LINUX_STUB_SMOKE failed $error\n$stack'); exit(1);
  }
}
