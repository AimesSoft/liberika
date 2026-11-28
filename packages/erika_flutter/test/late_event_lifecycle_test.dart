import 'dart:async';

import 'package:erika_flutter/erika_flutter.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  final binding = TestWidgetsFlutterBinding.ensureInitialized();

  test('native events during and after disposal remain safe', () async {
    const playerChannel = MethodChannel('erika_flutter/player');
    const eventsChannel = MethodChannel('erika_flutter/events');
    final messenger = binding.defaultBinaryMessenger;
    final nativeDispose = Completer<void>();
    messenger.setMockMethodCallHandler(playerChannel, (call) async {
      return switch (call.method) {
        'create' => 7,
        'dispose' => nativeDispose.future,
        _ => null,
      };
    });
    messenger.setMockMethodCallHandler(eventsChannel, (_) async => null);
    addTearDown(() {
      messenger.setMockMethodCallHandler(playerChannel, null);
      messenger.setMockMethodCallHandler(eventsChannel, null);
    });

    final player = ErikaPlayer();
    await player.ensureCreated();
    final received = <ErikaPlayerEvent>[];
    var closed = false;
    final subscription = player.events.listen(
      received.add,
      onDone: () => closed = true,
    );
    await Future<void>.delayed(Duration.zero);

    void emit(int playerId) {
      binding.channelBuffers.push(
        eventsChannel.name,
        const StandardMethodCodec().encodeSuccessEnvelope(<String, Object>{
          'playerId': playerId,
          'kind': 3,
          'positionMicros': 1000,
        }),
        (_) {},
      );
    }

    emit(7);
    await Future<void>.delayed(Duration.zero);
    expect(
      received,
      hasLength(1),
      reason: 'the test must reach the real event dispatcher',
    );

    final disposing = player.dispose();
    await Future<void>.delayed(Duration.zero);
    emit(7);
    await Future<void>.delayed(Duration.zero);
    expect(received, hasLength(2));

    nativeDispose.complete();
    await Future<void>.delayed(Duration.zero);
    await disposing;
    expect(closed, isTrue);
    for (var i = 0; i < 100; i++) {
      emit(7);
      emit(99);
    }
    await Future<void>.delayed(Duration.zero);
    expect(received, hasLength(2));
    await subscription.cancel();
  });
}
