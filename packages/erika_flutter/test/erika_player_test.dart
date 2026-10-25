import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:erika_flutter/erika_flutter.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  const playerChannel = MethodChannel('erika_flutter/player');
  const eventsChannel = MethodChannel('erika_flutter/events');

  late List<MethodCall> playerCalls;

  setUp(() {
    playerCalls = <MethodCall>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'dispose' => null,
        'registerSubtitleMemoryFont' => 41,
        'getSubtitleMemoryFontStatus' => <String, Object?>{
            'registeredCount': 2,
            'registeredBytes': 4096,
            'selectedCount': 1,
            'generation': 3,
            'selectedIds': <int>[41],
          },
        _ => null,
      };
    });
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(eventsChannel, (MethodCall call) async {
      return null;
    });
  });

  tearDown(() {
    debugDefaultTargetPlatformOverride = null;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, null);
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(eventsChannel, null);
  });

  testWidgets('macOS texture video view creates and attaches a Flutter texture',
      (WidgetTester tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    final player = ErikaPlayer();
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'createTexture' => 13,
        _ => null,
      };
    });

    await tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: SizedBox(
          width: 320,
          height: 180,
          child: ErikaTextureVideoView(player: player),
        ),
      ),
    );
    await tester.pump();
    await tester.pump();

    expect(find.byType(Texture), findsOneWidget);
    expect(playerCalls.map((call) => call.method), contains('createTexture'));
    expect(playerCalls.map((call) => call.method), contains('attachView'));

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pump();
    await player.dispose();
    debugDefaultTargetPlatformOverride = null;
  });

  testWidgets('macOS overlay video uses a native transparent compositing layer',
      (WidgetTester tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform_views, (
      MethodCall call,
    ) async {
      return null;
    });
    final player = ErikaPlayer(
      videoAlphaMode: ErikaVideoAlphaMode.packedAlphaRight,
    );
    try {
      await tester.pumpWidget(
        Directionality(
          textDirection: TextDirection.ltr,
          child: SizedBox(
            width: 320,
            height: 180,
            child: ErikaTextureVideoView(
              player: player,
              blendMode: BlendMode.overlay,
              opacity: 0.2,
            ),
          ),
        ),
      );
      await tester.pump();

      expect(find.byType(AppKitView), findsOneWidget);
      expect(find.byType(Texture), findsNothing);
      final view = tester.widget<AppKitView>(find.byType(AppKitView));
      expect(view.creationParams, <String, Object?>{
        'videoAlphaMode': ErikaVideoAlphaMode.packedAlphaRight.nativeValue,
        'blendMode': 'overlay',
        'opacity': 0.2,
      });
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pump();
      await player.dispose();
      debugDefaultTargetPlatformOverride = null;
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, null);
    }
  });

  test('default player lets native choose output mode', () async {
    final player = ErikaPlayer();

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    expect(createCall.arguments, isA<Map<Object?, Object?>>());
    expect(createCall.arguments as Map<Object?, Object?>, isEmpty);

    await player.dispose();
  });

  test('automatic output mode keeps the C ABI value 3', () {
    expect(ErikaOutputMode.auto.nativeValue, 3);
    expect(ErikaOutputMode.fromNativeValue(3), ErikaOutputMode.auto);
  });

  test('background playback is opt-in at player creation', () async {
    final player = ErikaPlayer(allowBackgroundPlayback: true);

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    expect(createCall.arguments, <String, Object?>{
      'allowBackgroundPlayback': true,
    });

    await player.dispose();
  });

  test('open forwards HTTP headers without exposing them elsewhere', () async {
    final player = ErikaPlayer();

    await player.open(
      'https://example.test/video.mkv',
      httpHeaders: <String, String>{'Authorization': 'Bearer secret'},
    );

    final call = playerCalls.singleWhere((call) => call.method == 'open');
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/video.mkv',
      'httpHeaders': <String, String>{'Authorization': 'Bearer secret'},
      'metadata': null,
    });
    await player.dispose();
  });

  test('subtitle memory font methods preserve typed data and IDs', () async {
    final player = ErikaPlayer();
    final data = Uint8List.fromList(<int>[0, 1, 2, 255]);

    expect(await player.registerSubtitleMemoryFont(data), 41);
    await player.selectSubtitleMemoryFonts(<int>[41, 42]);
    final status = await player.getSubtitleMemoryFontStatus();
    await player.clearSubtitleMemoryFonts();

    final register = playerCalls.singleWhere(
      (call) => call.method == 'registerSubtitleMemoryFont',
    );
    expect(register.arguments, <String, Object?>{'playerId': 7, 'data': data});
    final select = playerCalls.singleWhere(
      (call) => call.method == 'selectSubtitleMemoryFonts',
    );
    expect(select.arguments, <String, Object?>{
      'playerId': 7,
      'fontIds': <int>[41, 42],
    });
    expect(status.registeredCount, 2);
    expect(status.registeredBytes, 4096);
    expect(status.selectedCount, 1);
    expect(status.generation, 3);
    expect(status.selectedIds, <int>[41]);
    expect(
      playerCalls
          .singleWhere(
            (call) => call.method == 'clearSubtitleMemoryFonts',
          )
          .arguments,
      <String, Object?>{'playerId': 7},
    );
    await player.dispose();
  });

  test('subtitle memory font arguments are validated before channel calls',
      () async {
    final player = ErikaPlayer();

    await expectLater(
      player.registerSubtitleMemoryFont(Uint8List(0)),
      throwsArgumentError,
    );
    await expectLater(
      player.selectSubtitleMemoryFonts(<int>[1, 1]),
      throwsArgumentError,
    );
    await expectLater(
      player.selectSubtitleMemoryFonts(<int>[0]),
      throwsArgumentError,
    );
    expect(playerCalls, isEmpty);
    await player.dispose();
  });

  test('open omits null and empty HTTP headers', () async {
    final player = ErikaPlayer();

    await player.open('https://example.test/null.mkv');
    await player.open(
      'https://example.test/empty.mkv',
      httpHeaders: <String, String>{},
    );

    final openCalls = playerCalls
        .where((MethodCall call) => call.method == 'open')
        .toList(growable: false);
    expect(openCalls, hasLength(2));
    expect(openCalls[0].arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/null.mkv',
      'metadata': null,
    });
    expect(openCalls[1].arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/empty.mkv',
      'metadata': null,
    });

    await player.dispose();
  });

  test('open preserves multiple HTTP headers and empty values', () async {
    final player = ErikaPlayer();

    await player.open(
      'https://example.test/multiple.mkv',
      httpHeaders: <String, String>{
        'Authorization': 'Bearer token',
        'X-Empty': '',
        'X-Request-ID': 'request-123',
      },
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'open',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/multiple.mkv',
      'httpHeaders': <String, String>{
        'Authorization': 'Bearer token',
        'X-Empty': '',
        'X-Request-ID': 'request-123',
      },
      'metadata': null,
    });

    await player.dispose();
  });

  test('open forwards a positive HTTP read-ahead window', () async {
    final player = ErikaPlayer();

    await player.open(
      'https://example.test/read-ahead.mkv',
      httpReadAheadBytes: 2 * 1024 * 1024,
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'open',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/read-ahead.mkv',
      'httpReadAheadBytes': 2 * 1024 * 1024,
      'metadata': null,
    });

    await player.dispose();
  });

  test('open rejects a negative HTTP read-ahead before channel calls',
      () async {
    final player = ErikaPlayer();

    await expectLater(
      player.open(
        'https://example.test/read-ahead.mkv',
        httpReadAheadBytes: -1,
      ),
      throwsArgumentError,
    );

    expect(playerCalls, isEmpty);
    await player.dispose();
  });

  test('open forwards a positive HTTP rewind budget', () async {
    final player = ErikaPlayer();

    await player.open(
      'https://example.test/rewind.mkv',
      httpBackBufferBytes: 89 * 1024 * 1024,
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'open',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/rewind.mkv',
      'httpBackBufferBytes': 89 * 1024 * 1024,
      'metadata': null,
    });

    await player.dispose();
  });

  test('open rejects a negative HTTP rewind budget before channel calls',
      () async {
    final player = ErikaPlayer();

    await expectLater(
      player.open(
        'https://example.test/rewind.mkv',
        httpBackBufferBytes: -1,
      ),
      throwsArgumentError,
    );

    expect(playerCalls, isEmpty);
    await player.dispose();
  });

  test('open forwards both HTTP byte-count options together', () async {
    final player = ErikaPlayer();

    await player.open(
      'https://example.test/both.mkv',
      httpReadAheadBytes: 32 * 1024 * 1024,
      httpBackBufferBytes: 89 * 1024 * 1024,
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'open',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/both.mkv',
      'httpReadAheadBytes': 32 * 1024 * 1024,
      'httpBackBufferBytes': 89 * 1024 * 1024,
      'metadata': null,
    });

    await player.dispose();
  });

  test('media metadata is forwarded for system now playing info', () async {
    final player = ErikaPlayer();
    final artwork = Uint8List.fromList(<int>[1, 2, 3, 4]);
    final metadata = ErikaMediaMetadata(
      title: 'Episode 1',
      artist: 'Erika',
      album: 'Season 1',
      artwork: artwork,
    );

    await player.open('https://example.test/video.mkv', metadata: metadata);
    await player.setMediaMetadata(metadata);

    final openCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'open',
    );
    expect(openCall.arguments, <String, Object?>{
      'playerId': 7,
      'uri': 'https://example.test/video.mkv',
      'metadata': <String, Object>{
        'title': 'Episode 1',
        'artist': 'Erika',
        'album': 'Season 1',
        'artwork': artwork,
      },
    });
    final metadataCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setMediaMetadata',
    );
    expect(metadataCall.arguments, <String, Object?>{
      'playerId': 7,
      'metadata': <String, Object>{
        'title': 'Episode 1',
        'artist': 'Erika',
        'album': 'Season 1',
        'artwork': artwork,
      },
    });

    await player.dispose();
  });

  test('system media navigation capabilities are forwarded', () async {
    final player = ErikaPlayer();

    await player.setSystemMediaNavigation(
      previousEnabled: true,
      nextEnabled: false,
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setSystemMediaNavigation',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'previousEnabled': true,
      'nextEnabled': false,
    });

    await player.dispose();
  });

  test('apple EDR output mode is passed to native create', () async {
    final player = ErikaPlayer(
      outputMode: ErikaOutputMode.appleEdr,
      edrHeadroom: 4.0,
    );

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(arguments['outputMode'], ErikaOutputMode.appleEdr.nativeValue);
    expect(arguments['edrHeadroom'], 4.0);

    await player.dispose();
  });

  test('extended-linear output mode is passed to native create', () async {
    final player = ErikaPlayer(
      outputMode: ErikaOutputMode.extendedLinear,
      edrHeadroom: 3.0,
    );

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(arguments['outputMode'], ErikaOutputMode.extendedLinear.nativeValue);
    expect(arguments['edrHeadroom'], 3.0);

    await player.dispose();
  });

  test('extended-linear output defaults shader headroom to four', () async {
    final player = ErikaPlayer(outputMode: ErikaOutputMode.extendedLinear);

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(arguments['edrHeadroom'], 4.0);

    await player.dispose();
  });

  test('invalid explicit output headroom is rejected before native create', () {
    expect(
      () => ErikaPlayer(
        outputMode: ErikaOutputMode.extendedLinear,
        edrHeadroom: 0.5,
      ),
      throwsArgumentError,
    );
    expect(
      () => ErikaPlayer(
        outputMode: ErikaOutputMode.extendedLinear,
        edrHeadroom: double.nan,
      ),
      throwsArgumentError,
    );
    expect(playerCalls, isEmpty);
  });

  test('initial upscaler mode is passed to native create', () async {
    final player = ErikaPlayer(upscaler: ErikaUpscalerMode.artCnnC4F16Ds);

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(arguments['upscaler'], ErikaUpscalerMode.artCnnC4F16Ds.nativeValue);

    await player.dispose();
  });

  test('packed alpha mode is passed to native create', () async {
    final player = ErikaPlayer(
      videoAlphaMode: ErikaVideoAlphaMode.packedAlphaRight,
    );

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(
      arguments['videoAlphaMode'],
      ErikaVideoAlphaMode.packedAlphaRight.nativeValue,
    );

    await player.dispose();
  });

  test('HDR debug flag is passed to native create when enabled', () async {
    final player = ErikaPlayer(hdrDebug: true);

    expect(await player.ensureCreated(), 7);

    final createCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'create',
    );
    final arguments = createCall.arguments as Map<Object?, Object?>;
    expect(arguments['hdrDebug'], true);

    await player.dispose();
  });

  test(
    'dispose waits for delayed create and blocks pending player calls',
    () async {
      final createCompleter = Completer<int>();
      final disposeCompleter = Completer<void>();
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
        playerCalls.add(call);
        return switch (call.method) {
          'create' => createCompleter.future,
          'dispose' => disposeCompleter.future,
          _ => null,
        };
      });
      final player = ErikaPlayer();

      final openFuture = player.open('/tmp/delayed.mkv');
      final openExpectation = expectLater(
        openFuture,
        throwsA(isA<StateError>()),
      );
      await Future<void>.delayed(Duration.zero);
      expect(
        playerCalls.where((MethodCall call) => call.method == 'create'),
        hasLength(1),
      );

      final firstDispose = player.dispose();
      final secondDispose = player.dispose();
      expect(identical(firstDispose, secondDispose), isTrue);
      expect(() => player.ensureCreated(), throwsA(isA<StateError>()));

      createCompleter.complete(41);
      await openExpectation;
      await Future<void>.delayed(Duration.zero);

      expect(
        playerCalls.where((MethodCall call) => call.method == 'open'),
        isEmpty,
      );
      final disposeCalls = playerCalls
          .where((MethodCall call) => call.method == 'dispose')
          .toList(growable: false);
      expect(disposeCalls, hasLength(1));
      expect(disposeCalls.single.arguments, <String, Object?>{'playerId': 41});

      var cleanupCompleted = false;
      final observedDispose = firstDispose.whenComplete(
        () => cleanupCompleted = true,
      );
      await Future<void>.delayed(Duration.zero);
      expect(cleanupCompleted, isFalse);

      disposeCompleter.complete();
      await Future.wait(<Future<void>>[observedDispose, secondDispose]);
      expect(cleanupCompleted, isTrue);
      expect(player.id, isNull);
      await expectLater(player.play(), throwsA(isA<StateError>()));
    },
  );

  test('external subtitle add returns native track id', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'addExternalSubtitle' => 1000001,
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final trackId = await player.addExternalSubtitle('/tmp/subs.srt');

    expect(trackId, 1000001);
    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'addExternalSubtitle',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'uri': '/tmp/subs.srt',
    });

    await player.dispose();
  });

  test('external subtitle remove forwards track id', () async {
    final player = ErikaPlayer();

    await player.removeSubtitleTrack(1000001);

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'removeSubtitleTrack',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'trackId': 1000001,
    });

    await player.dispose();
  });

  test('screenshot forwards optional capture size', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'screenshot' => Uint8List.fromList(<int>[1, 2, 3, 4]),
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final bytes = await player.screenshot(width: 320, height: 180);

    expect(bytes, <int>[1, 2, 3, 4]);
    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'screenshot',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'width': 320,
      'height': 180,
    });

    await player.dispose();
  });

  test('screenshot returns null when native has no current frame', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      return switch (call.method) {
        'create' => 7,
        'screenshot' => null,
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    expect(await player.screenshot(width: 320, height: 180), isNull);

    await player.dispose();
  });

  test('macOS GIF export keeps using the plugin-owned runtime', () async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      if (call.method == 'exportGif') {
        return <String, Object?>{
          'outputPath': '/tmp/output.gif',
          'width': 320,
          'height': 180,
          'frameCount': 50,
          'fileSize': 4096,
        };
      }
      return null;
    });

    final result = await ErikaPlayer.exportGif(
      const ErikaGifExportOptions(
        inputUri: '/tmp/input.mp4',
        outputPath: '/tmp/output.gif',
        start: Duration(seconds: 1),
        end: Duration(seconds: 6),
        framesPerSecond: 10,
        outputWidth: 320,
        outputHeight: 180,
      ),
    );

    expect(result.frameCount, 50);
    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'exportGif',
    );
    expect((call.arguments as Map<Object?, Object?>)['framesPerSecond'], 10);
  });

  test('macOS frame export runs in the plugin and returns its bytes', () async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      if (call.method == 'exportFrame') {
        return <String, Object?>{
          'width': 640,
          'height': 273,
          'format': 1,
          'bytes': Uint8List.fromList(<int>[0x89, 0x50, 0x4e, 0x47]),
          'timestampMicros': 59958000,
          'durationMicros': 888032000,
          'sourceWidth': 4096,
          'sourceHeight': 1744,
        };
      }
      return null;
    });

    final frame = await ErikaPlayer.exportFrame(
      '/tmp/input.mkv',
      position: const Duration(seconds: 60),
      width: 640,
      height: 640,
    );

    expect(frame.format, ErikaFrameFormat.png);
    expect((frame.width, frame.height), (640, 273));
    expect((frame.sourceWidth, frame.sourceHeight), (4096, 1744));
    expect(frame.timestamp, const Duration(microseconds: 59958000));
    expect(frame.duration, const Duration(microseconds: 888032000));
    expect(frame.bytes, <int>[0x89, 0x50, 0x4e, 0x47]);
    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'exportFrame',
    );
    expect(call.arguments, <String, Object?>{
      'uri': '/tmp/input.mkv',
      'positionMillis': 60000,
      'width': 640,
      'height': 640,
      'format': 1,
    });
  });

  test('macOS frame export reports liberika failures as errors', () async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      throw PlatformException(
        code: call.arguments is Map &&
                (call.arguments as Map)['positionMillis'] == 1
            ? 'ERIKA_UNSUPPORTED'
            : 'ERIKA_ERROR',
        message: 'Frame export failed: position 900s is not before the end',
      );
    });

    await expectLater(
      ErikaPlayer.exportFrame('/tmp/input.mkv',
          position: const Duration(seconds: 900)),
      throwsA(isA<StateError>().having(
          (error) => error.message, 'message', contains('not before the end'))),
    );
    await expectLater(
      ErikaPlayer.exportFrame('/tmp/input.mkv',
          position: const Duration(milliseconds: 1)),
      throwsA(isA<UnsupportedError>()),
    );
    await expectLater(
      ErikaPlayer.exportFrame('/tmp/input.mkv',
          position: Duration.zero, width: -1),
      throwsA(isA<ArgumentError>()),
    );
  });

  test('macOS frame export session keeps the media open across pictures',
      () async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    var closed = false;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      switch (call.method) {
        case 'openFrameExportSession':
          return 41;
        case 'exportSessionFrame':
          final arguments = call.arguments as Map<Object?, Object?>;
          if (arguments['positionMillis'] == 900000) {
            throw PlatformException(
              code: 'ERIKA_ERROR',
              message: 'Frame export failed: position 900s is not before the end',
            );
          }
          return <String, Object?>{
            'width': 640,
            'height': 273,
            'format': arguments['format'],
            'bytes': Uint8List.fromList(<int>[1, 2, 3]),
            'timestampMicros': (arguments['positionMillis']! as int) * 1000,
            'durationMicros': 888032000,
            'sourceWidth': 3840,
            'sourceHeight': 1632,
            'toneMapped': true,
          };
        case 'closeFrameExportSession':
          closed = true;
          return null;
      }
      return null;
    });

    final session = await ErikaFrameExportSession.open(
      '/tmp/input.mkv',
      httpHeaders: const <String, String>{'X-Test': '1'},
    );
    final first = await session.frameAt(
      const Duration(seconds: 60),
      width: 640,
      format: ErikaFrameFormat.rgba,
    );
    final second = await session.frameAt(const Duration(milliseconds: 60100));
    expect(first.toneMapped, isTrue);
    expect(first.format, ErikaFrameFormat.rgba);
    expect(second.timestamp, const Duration(milliseconds: 60100));
    expect((second.sourceWidth, second.sourceHeight), (3840, 1632));
    await expectLater(
      session.frameAt(const Duration(seconds: 900)),
      throwsA(isA<StateError>().having(
          (error) => error.message, 'message', contains('not before the end'))),
    );
    await expectLater(
      session.frameAt(Duration.zero, width: -1),
      throwsA(isA<ArgumentError>()),
    );
    await session.close();
    await session.close();
    expect(closed, isTrue);
    await expectLater(
        session.frameAt(Duration.zero), throwsA(isA<StateError>()));

    final calls = playerCalls.map((MethodCall call) => call.method).toList();
    expect(calls.where((name) => name == 'openFrameExportSession'), hasLength(1));
    expect(calls.where((name) => name == 'closeFrameExportSession'), hasLength(1));
    final open = playerCalls.firstWhere(
      (MethodCall call) => call.method == 'openFrameExportSession',
    );
    expect(open.arguments, <String, Object?>{
      'uri': '/tmp/input.mkv',
      'httpHeaders': <String, String>{'X-Test': '1'},
    });
    final frame = playerCalls.firstWhere(
      (MethodCall call) => call.method == 'exportSessionFrame',
    );
    expect(frame.arguments, <String, Object?>{
      'sessionId': 41,
      'positionMillis': 60000,
      'width': 640,
      'height': 0,
      'format': 0,
    });
  });

  test('exportFrames opens the media once for all positions and closes it',
      () async {
    debugDefaultTargetPlatformOverride = TargetPlatform.macOS;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      if (call.method == 'openFrameExportSession') {
        return 5;
      }
      if (call.method == 'exportSessionFrame') {
        final arguments = call.arguments as Map<Object?, Object?>;
        if (arguments['positionMillis'] == 3000) {
          throw PlatformException(code: 'ERIKA_ERROR', message: 'cannot decode');
        }
        return <String, Object?>{
          'width': 16,
          'height': 9,
          'format': 1,
          'bytes': Uint8List(4),
          'timestampMicros': 0,
        };
      }
      return null;
    });

    final frames = await ErikaPlayer.exportFrames(
      '/tmp/input.mkv',
      positions: const <Duration>[Duration(seconds: 1), Duration(seconds: 2)],
    );
    expect(frames, hasLength(2));
    await expectLater(
      ErikaPlayer.exportFrames('/tmp/input.mkv',
          positions: const <Duration>[Duration(seconds: 1), Duration(seconds: 3)]),
      throwsA(isA<StateError>()),
    );
    final methods = playerCalls.map((MethodCall call) => call.method).toList();
    expect(methods.where((name) => name == 'openFrameExportSession'), hasLength(2));
    expect(methods.where((name) => name == 'closeFrameExportSession'), hasLength(2),
        reason: 'the media is released even when a picture fails');
  });

  test('GIF export rejects values outside the native ABI limits', () async {
    Future<void> expectRejected(ErikaGifExportOptions options) async {
      await expectLater(
        ErikaPlayer.exportGif(options),
        throwsA(isA<ArgumentError>()),
      );
    }

    await expectRejected(
      const ErikaGifExportOptions(
        inputUri: '/tmp/input.mp4',
        outputPath: '/tmp/output.gif',
        start: Duration.zero,
        end: Duration(seconds: 1),
        framesPerSecond: 61,
        outputWidth: 320,
        outputHeight: 180,
      ),
    );
    await expectRejected(
      const ErikaGifExportOptions(
        inputUri: '/tmp/input.mp4',
        outputPath: '/tmp/output.gif',
        start: Duration.zero,
        end: Duration(seconds: 1),
        framesPerSecond: 10,
        outputWidth: 8193,
        outputHeight: 180,
      ),
    );
    await expectRejected(
      const ErikaGifExportOptions(
        inputUri: '/tmp/input.mp4',
        outputPath: '/tmp/output.gif',
        start: Duration.zero,
        end: Duration(seconds: 1),
        framesPerSecond: 10,
        outputWidth: 320,
        outputHeight: 180,
        loopCount: 0x10000,
      ),
    );
  });

  test('track selection methods forward nullable track ids', () async {
    final player = ErikaPlayer();

    await player.selectAudioTrack(2);
    await player.selectSubtitleTrack(null);

    final audioCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'selectAudioTrack',
    );
    expect(audioCall.arguments, <String, Object?>{'playerId': 7, 'trackId': 2});

    final subtitleCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'selectSubtitleTrack',
    );
    expect(subtitleCall.arguments, <String, Object?>{
      'playerId': 7,
      'trackId': null,
    });

    await player.dispose();
  });

  test('playback rate is forwarded to native player clock', () async {
    final player = ErikaPlayer();

    await player.setPlaybackRate(1.5);

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setPlaybackRate',
    );
    expect(call.arguments, <String, Object?>{'playerId': 7, 'rate': 1.5});

    await player.dispose();
  });

  test('debug HUD toggle is forwarded to native presenter', () async {
    final player = ErikaPlayer();

    await player.setDebugHudEnabled(true);
    await player.setDebugHudEnabled(false);

    final calls = playerCalls
        .where((MethodCall call) => call.method == 'setDebugHudEnabled')
        .toList();
    expect(calls, hasLength(2));
    expect(calls.first.arguments, <String, Object?>{
      'playerId': 7,
      'enabled': true,
    });
    expect(calls.last.arguments, <String, Object?>{
      'playerId': 7,
      'enabled': false,
    });

    await player.dispose();
  });

  test('window overlay methods forward surface geometry', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'attachOverlay' => ErikaPlayer.windowOverlayViewId,
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final viewId = await player.attachWindowOverlay(
      flutterViewId: 17,
      secondaryWindow: true,
      blendMode: 'overlay',
      opacity: 0.75,
    );
    await player.setWindowOverlayFrame(
      frame: const Rect.fromLTWH(10, 20, 320, 180),
      visible: true,
      generation: 42,
      flutterViewId: 17,
      secondaryWindow: true,
      debugLabel: 'episode.mkv',
      blendMode: 'overlay',
      opacity: 0.75,
    );
    await player.detachWindowOverlay(generation: 42);

    expect(viewId, ErikaPlayer.windowOverlayViewId);
    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'attachOverlay')
          .arguments,
      <String, Object?>{
        'playerId': 7,
        'flutterViewId': 17,
        'secondaryWindow': true,
        'blendMode': 'overlay',
        'opacity': 0.75,
      },
    );
    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'setOverlayFrame')
          .arguments,
      <String, Object?>{
        'playerId': 7,
        'viewId': ErikaPlayer.windowOverlayViewId,
        'generation': 42,
        'x': 10.0,
        'y': 20.0,
        'width': 320.0,
        'height': 180.0,
        'visible': true,
        'flutterViewId': 17,
        'secondaryWindow': true,
        'debugLabel': 'episode.mkv',
        'blendMode': 'overlay',
        'opacity': 0.75,
      },
    );
    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'detachOverlay')
          .arguments,
      <String, Object?>{'playerId': 7, 'generation': 42},
    );

    await player.dispose();
  });

  test('window overlay explicitly targets the main window by default',
      () async {
    final player = ErikaPlayer();

    await player.attachWindowOverlay(flutterViewId: 3);
    await player.setWindowOverlayFrame(
      frame: const Rect.fromLTWH(0, 0, 640, 360),
      visible: true,
      generation: 7,
      flutterViewId: 3,
    );

    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'attachOverlay')
          .arguments,
      <String, Object?>{
        'playerId': 7,
        'flutterViewId': 3,
        'secondaryWindow': false,
      },
    );
    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'setOverlayFrame')
          .arguments,
      containsPair('secondaryWindow', false),
    );

    await player.dispose();
  });

  testWidgets('window overlay reports its transformed FittedBox bounds', (
    WidgetTester tester,
  ) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.windows;
    final player = ErikaPlayer();
    try {
      await tester.pumpWidget(
        Directionality(
          textDirection: TextDirection.ltr,
          child: Center(
            child: SizedBox(
              width: 640,
              height: 360,
              child: FittedBox(
                fit: BoxFit.fill,
                child: SizedBox(
                  width: 320,
                  height: 180,
                  child: ErikaWindowOverlayVideoView(player: player),
                ),
              ),
            ),
          ),
        ),
      );
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 20));

      final frameCalls = playerCalls
          .where((MethodCall call) => call.method == 'setOverlayFrame')
          .toList();
      expect(frameCalls, isNotEmpty);
      final arguments = frameCalls.last.arguments as Map<Object?, Object?>;
      expect(arguments['width'], closeTo(640.0, 0.01));
      expect(arguments['height'], closeTo(360.0, 0.01));
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pump();
      await player.dispose();
      debugDefaultTargetPlatformOverride = null;
    }
  });

  testWidgets('window overlay uses the Android TextureView platform view', (
    WidgetTester tester,
  ) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.android;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform_views, (
      MethodCall call,
    ) async {
      final arguments = call.arguments as Map<Object?, Object?>?;
      return switch (call.method) {
        'create' => 1,
        'resize' => <String, Object?>{
            'width': arguments!['width'],
            'height': arguments['height'],
          },
        _ => null,
      };
    });
    final player = ErikaPlayer();
    try {
      await tester.pumpWidget(
        Directionality(
          textDirection: TextDirection.ltr,
          child: SizedBox(
            width: 320,
            height: 180,
            child: ErikaWindowOverlayVideoView(
              player: player,
              debugLabel: 'android-video',
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();

      expect(find.byType(AndroidView), findsOneWidget);
      final attachCall = playerCalls.singleWhere(
        (MethodCall call) => call.method == 'attachView',
      );
      final attachArguments = attachCall.arguments as Map<Object?, Object?>;
      expect(attachArguments['playerId'], 7);
      expect(attachArguments['viewId'], isA<int>());
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pumpAndSettle();
      await player.dispose();
      debugDefaultTargetPlatformOverride = null;
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, null);
    }
  });

  testWidgets(
    'extended-linear Android view forces Hybrid Composition SurfaceView',
    (WidgetTester tester) async {
      debugDefaultTargetPlatformOverride = TargetPlatform.android;
      final platformViewCalls = <MethodCall>[];
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, (
        MethodCall call,
      ) async {
        platformViewCalls.add(call);
        return null;
      });
      final player = ErikaPlayer(
        outputMode: ErikaOutputMode.extendedLinear,
        edrHeadroom: 4.0,
      );
      try {
        await tester.pumpWidget(
          Directionality(
            textDirection: TextDirection.ltr,
            child: SizedBox(
              width: 320,
              height: 180,
              child: ErikaVideoView(
                player: player,
                debugLabel: 'android-hdr-video',
              ),
            ),
          ),
        );
        await tester.pumpAndSettle();

        expect(find.byType(PlatformViewLink), findsOneWidget);
        expect(find.byType(AndroidView), findsNothing);
        final createCall = platformViewCalls.singleWhere(
          (MethodCall call) => call.method == 'create',
        );
        final createArguments = createCall.arguments as Map<Object?, Object?>;
        expect(createArguments['viewType'], 'erika_flutter/hdr_video_view');
        expect(createArguments['hybrid'], isTrue);
        final encodedParams = createArguments['params'] as Uint8List;
        final creationParams = const StandardMessageCodec().decodeMessage(
          ByteData.sublistView(encodedParams),
        ) as Map<Object?, Object?>;
        expect(
          creationParams['outputMode'],
          ErikaOutputMode.extendedLinear.nativeValue,
        );
        expect(creationParams['requestedHdrHeadroom'], 4.0);
        expect(creationParams['composition'], 'hybrid');

        final attachCall = playerCalls.singleWhere(
          (MethodCall call) => call.method == 'attachView',
        );
        final attachArguments = attachCall.arguments as Map<Object?, Object?>;
        expect(attachArguments['playerId'], 7);
        expect(attachArguments['viewId'], createArguments['id']);
      } finally {
        await tester.pumpWidget(const SizedBox.shrink());
        await tester.pumpAndSettle();
        await player.dispose();
        debugDefaultTargetPlatformOverride = null;
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
            .setMockMethodCallHandler(SystemChannels.platform_views, null);
      }
    },
  );

  testWidgets('changing extended-linear headroom recreates the Android view', (
    WidgetTester tester,
  ) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.android;
    var nextPlayerId = 7;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => nextPlayerId++,
        'dispose' => null,
        _ => null,
      };
    });
    final platformViewCalls = <MethodCall>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform_views, (
      MethodCall call,
    ) async {
      platformViewCalls.add(call);
      return null;
    });
    final automatic = ErikaPlayer(outputMode: ErikaOutputMode.extendedLinear);
    final explicit = ErikaPlayer(
      outputMode: ErikaOutputMode.extendedLinear,
      edrHeadroom: 2.5,
    );
    try {
      Widget view(ErikaPlayer player) => Directionality(
            textDirection: TextDirection.ltr,
            child: SizedBox(
              width: 320,
              height: 180,
              child: ErikaVideoView(player: player),
            ),
          );

      await tester.pumpWidget(view(automatic));
      await tester.pumpAndSettle();
      await tester.pumpWidget(view(explicit));
      await tester.pumpAndSettle();

      final createCalls = platformViewCalls
          .where((MethodCall call) => call.method == 'create')
          .toList(growable: false);
      expect(createCalls, hasLength(2));
      final creationParams = createCalls.map((MethodCall call) {
        final arguments = call.arguments as Map<Object?, Object?>;
        return const StandardMessageCodec().decodeMessage(
          ByteData.sublistView(arguments['params'] as Uint8List),
        ) as Map<Object?, Object?>;
      }).toList(growable: false);
      expect(creationParams[0]['requestedHdrHeadroom'], 0.0);
      expect(creationParams[1]['requestedHdrHeadroom'], 2.5);
      expect(
        playerCalls.where((MethodCall call) => call.method == 'attachView'),
        hasLength(2),
      );
      expect(
        playerCalls.where((MethodCall call) => call.method == 'detachView'),
        isNotEmpty,
      );
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pumpAndSettle();
      await automatic.dispose();
      await explicit.dispose();
      debugDefaultTargetPlatformOverride = null;
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, null);
    }
  });

  testWidgets('Android player switch retries a transient attach failure', (
    WidgetTester tester,
  ) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.android;
    var nextPlayerId = 7;
    var replacementAttachAttempts = 0;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      final arguments = call.arguments as Map<Object?, Object?>?;
      return switch (call.method) {
        'create' => nextPlayerId++,
        'attachView' when arguments?['playerId'] == 8 =>
          ++replacementAttachAttempts < 3
              ? throw PlatformException(
                  code: 'ERIKA_ERROR',
                  message: 'transient detach recovery',
                )
              : null,
        'dispose' => null,
        _ => null,
      };
    });
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform_views, (
      MethodCall call,
    ) async {
      final arguments = call.arguments as Map<Object?, Object?>?;
      return switch (call.method) {
        'create' => 1,
        'resize' => <String, Object?>{
            'width': arguments!['width'],
            'height': arguments['height'],
          },
        _ => null,
      };
    });
    final initial = ErikaPlayer();
    final replacement = ErikaPlayer();
    try {
      Widget view(ErikaPlayer player) => Directionality(
            textDirection: TextDirection.ltr,
            child: SizedBox(
              width: 320,
              height: 180,
              child: ErikaVideoView(player: player),
            ),
          );

      await tester.pumpWidget(view(initial));
      await tester.pumpAndSettle();
      await tester.pumpWidget(view(replacement));
      await tester.pumpAndSettle();

      expect(replacementAttachAttempts, 3);
      expect(
        playerCalls.where((MethodCall call) {
          if (call.method != 'detachView') {
            return false;
          }
          final arguments = call.arguments as Map<Object?, Object?>;
          return arguments['playerId'] == 7;
        }),
        isNotEmpty,
      );
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pumpAndSettle();
      await initial.dispose();
      await replacement.dispose();
      debugDefaultTargetPlatformOverride = null;
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, null);
    }
  });

  testWidgets(
    'stale Android platform view creation cannot replace new output config',
    (WidgetTester tester) async {
      debugDefaultTargetPlatformOverride = TargetPlatform.android;
      var nextPlayerId = 7;
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
        playerCalls.add(call);
        return switch (call.method) {
          'create' => nextPlayerId++,
          'dispose' => null,
          _ => null,
        };
      });
      final createCompleters = <Completer<void>>[];
      final createViewIds = <int>[];
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform_views, (
        MethodCall call,
      ) {
        if (call.method != 'create') {
          return Future<Object?>.value(null);
        }
        final arguments = call.arguments as Map<Object?, Object?>;
        final completer = Completer<void>();
        createViewIds.add(arguments['id']! as int);
        createCompleters.add(completer);
        return completer.future.then<Object?>((_) => null);
      });
      final automatic = ErikaPlayer(outputMode: ErikaOutputMode.extendedLinear);
      final explicit = ErikaPlayer(
        outputMode: ErikaOutputMode.extendedLinear,
        edrHeadroom: 2.5,
      );
      try {
        expect(await automatic.ensureCreated(), 7);
        expect(await explicit.ensureCreated(), 8);
        Widget view(ErikaPlayer player) => Directionality(
              textDirection: TextDirection.ltr,
              child: SizedBox(
                width: 320,
                height: 180,
                child: ErikaVideoView(player: player),
              ),
            );

        await tester.pumpWidget(view(automatic));
        await tester.pump();
        expect(createCompleters, hasLength(1));

        await tester.pumpWidget(view(explicit));
        await tester.pump();
        expect(createCompleters, hasLength(2));

        createCompleters[1].complete();
        await tester.pump();
        createCompleters[0].complete();
        await tester.pump();

        final attachCalls = playerCalls
            .where((MethodCall call) => call.method == 'attachView')
            .toList(growable: false);
        expect(attachCalls, hasLength(1));
        expect(attachCalls.single.arguments, <String, Object?>{
          'playerId': 8,
          'viewId': createViewIds[1],
        });
      } finally {
        for (final completer in createCompleters) {
          if (!completer.isCompleted) {
            completer.complete();
          }
        }
        await tester.pumpWidget(const SizedBox.shrink());
        await tester.pumpAndSettle();
        await automatic.dispose();
        await explicit.dispose();
        debugDefaultTargetPlatformOverride = null;
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
            .setMockMethodCallHandler(SystemChannels.platform_views, null);
      }
    },
  );

  test('upscaler mode is forwarded to native presenter', () async {
    final player = ErikaPlayer();

    await player.setUpscaler(ErikaUpscalerMode.artCnnC4F16);

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setUpscaler',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'mode': ErikaUpscalerMode.artCnnC4F16.nativeValue,
    });

    await player.dispose();
  });

  test('DS upscaler status mode is decoded from native presenter', () {
    expect(
      ErikaUpscalerMode.fromNativeValue(3),
      ErikaUpscalerMode.artCnnC4F16Ds,
    );
  });

  test('upscaler status is decoded from native presenter', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'getUpscalerStatus' => <String, Object?>{
            'requestedMode': ErikaUpscalerMode.artCnnC4F32.nativeValue,
            'activeBackend':
                ErikaUpscalerBackendStatus.simdgroupMatrix.nativeValue,
            'fallbackCount': 1,
            'upscaledFrames': 42,
            'lastEncodeMicros': 1200,
            'lastGpuMicros': 3400,
          },
        'dispose' => null,
        _ => null,
      };
    });

    final player = ErikaPlayer();

    final status = await player.getUpscalerStatus();

    expect(status.requestedMode, ErikaUpscalerMode.artCnnC4F32);
    expect(status.activeBackend, ErikaUpscalerBackendStatus.simdgroupMatrix);
    expect(status.fallbackCount, 1);
    expect(status.upscaledFrames, 42);
    expect(status.lastEncodeDuration, const Duration(microseconds: 1200));
    expect(status.lastGpuDuration, const Duration(microseconds: 3400));

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'getUpscalerStatus',
    );
    expect(call.arguments, <String, Object?>{'playerId': 7});

    await player.dispose();
  });

  test(
    'extended-linear output status is decoded from native presenter',
    () async {
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
        playerCalls.add(call);
        return switch (call.method) {
          'create' => 7,
          'getOutputStatus' => <String, Object?>{
              'requestedMode': ErikaOutputMode.extendedLinear.nativeValue,
              'activeEncoding': ErikaActiveOutputEncoding
                  .androidExtendedLinearScRgb.nativeValue,
              'surfaceFormat':
                  ErikaOutputSurfaceFormat.sixteenBitFloat.nativeValue,
              'nativeDataSpace': 0x18410000,
              'requestedHeadroom': 4.0,
              'activeHeadroom': 3.5,
              'activeHeadroomKnown': true,
              'extendedLinearActive': true,
              'fallbackReason': ErikaOutputFallbackReason.none.nativeValue,
              'fallbackCount': 0,
              'dataSpaceFailures': 0,
              'headroomUpdates': 2,
              'extendedLinearFrames': 42,
            },
          'dispose' => null,
          _ => null,
        };
      });

      final player = ErikaPlayer(outputMode: ErikaOutputMode.extendedLinear);
      final status = await player.getOutputStatus();

      expect(status.requestedMode, ErikaOutputMode.extendedLinear);
      expect(
        status.activeEncoding,
        ErikaActiveOutputEncoding.androidExtendedLinearScRgb,
      );
      expect(status.surfaceFormat, ErikaOutputSurfaceFormat.sixteenBitFloat);
      expect(status.nativeDataSpace, 0x18410000);
      expect(status.activeHeadroom, 3.5);
      expect(status.extendedLinearActive, isTrue);
      expect(status.fallbackReason, ErikaOutputFallbackReason.none);
      expect(status.extendedLinearFrames, 42);

      final call = playerCalls.singleWhere(
        (MethodCall call) => call.method == 'getOutputStatus',
      );
      expect(call.arguments, <String, Object?>{'playerId': 7});

      await player.dispose();
    },
  );

  test('renderer resource status is decoded from native presenter', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'getResourceStatus' => <String, Object?>{
            'deviceCurrentAllocatedBytes': 1000000000,
            'deviceRecommendedWorkingSetBytes': 8000000000,
            'drawableEstimatedBytes': 99532800,
            'videoFrameBytes': 24883200,
            'overlayAtlasBytes': 2097152,
            'danmakuAtlasBytes': 4194304,
            'danmakuVertexBufferBytes': 65536,
            'upscalerBytes': 0,
            'rendererTrackedBytes': 130772992,
            'presenterCpuDanmakuAtlasBytes': 4194304,
            'drawableCount': 3,
            'outputModeSwitches': 1,
          },
        'dispose' => null,
        _ => null,
      };
    });

    final player = ErikaPlayer();
    final status = await player.getResourceStatus();

    expect(status.deviceCurrentAllocatedBytes, 1000000000);
    expect(status.drawableEstimatedBytes, 99532800);
    expect(status.videoFrameBytes, 24883200);
    expect(status.rendererTrackedBytes, 130772992);
    expect(status.drawableCount, 3);
    expect(status.outputModeSwitches, 1);

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'getResourceStatus',
    );
    expect(call.arguments, <String, Object?>{'playerId': 7});

    await player.dispose();
  });

  test('subtitle style forwards font and colors with defaults', () async {
    final player = ErikaPlayer();

    await player.setSubtitleStyle(
      fontFamily: 'Erika Sans',
      fontFilePath: '/tmp/subtitle.otf',
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setSubtitleStyle',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'fontFamily': 'Erika Sans',
      'fontFilePath': '/tmp/subtitle.otf',
      'primaryColorRgba': kErikaDefaultSubtitlePrimaryColorRgba,
      'outlineColorRgba': kErikaDefaultSubtitleOutlineColorRgba,
      'fontSize': kErikaDefaultSubtitleFontSize,
      'outlineWidth': kErikaDefaultSubtitleOutlineWidth,
      'bold': false,
      'italic': false,
      'underline': false,
      'strikeOut': false,
      'spacing': 0.0,
      'scaleXPercent': 100.0,
      'scaleYPercent': 100.0,
      'borderStyle': 1,
      'shadowDepth': 0.0,
      'blur': 0.0,
      'alignment': 2,
      'marginLeft': 48,
      'marginRight': 48,
      'marginVertical': 54,
      'overrideMask': 0,
    });

    await player.dispose();
  });

  test('subtitle style keeps previously applied fields', () async {
    final player = ErikaPlayer();

    await player.setSubtitleStyle(fontFamily: 'Erika Sans');
    await player.setSubtitleStyle(
      primaryColorRgba: 0xFF0000FF,
      fontSize: 64.0,
      outlineWidth: 4.0,
      bold: true,
      italic: true,
      underline: true,
      strikeOut: true,
      spacing: 2.5,
      scaleXPercent: 110.0,
      scaleYPercent: 90.0,
      borderStyle: 3,
      shadowDepth: 5.0,
      blur: 1.5,
      alignment: 8,
      marginLeft: 24,
      marginRight: 36,
      marginVertical: 42,
      overrideMask: kErikaSubtitleOverrideAll,
    );
    await player.setSubtitleStyle(marginVertical: 84);

    final calls = playerCalls
        .where((MethodCall call) => call.method == 'setSubtitleStyle')
        .toList();
    expect(calls, hasLength(3));
    expect(calls.last.arguments, <String, Object?>{
      'playerId': 7,
      'fontFamily': 'Erika Sans',
      'fontFilePath': '',
      'primaryColorRgba': 0xFF0000FF,
      'outlineColorRgba': kErikaDefaultSubtitleOutlineColorRgba,
      'fontSize': 64.0,
      'outlineWidth': 4.0,
      'bold': true,
      'italic': true,
      'underline': true,
      'strikeOut': true,
      'spacing': 2.5,
      'scaleXPercent': 110.0,
      'scaleYPercent': 90.0,
      'borderStyle': 3,
      'shadowDepth': 5.0,
      'blur': 1.5,
      'alignment': 8,
      'marginLeft': 24,
      'marginRight': 36,
      'marginVertical': 84,
      'overrideMask': kErikaSubtitleOverrideAll,
    });

    await player.dispose();
  });

  test('danmaku config forwards block words as json', () async {
    final player = ErikaPlayer();

    await player.setDanmakuConfig(
      maxQuantity: 80,
      shadowStyle: 3,
      customFontFamily: 'DanmakuRuntime_abc',
      customFontFilePath: '/tmp/danmaku.otf',
      blockWords: <String>['spoiler', 'regex/[0-9]+/'],
    );

    final call = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setDanmakuConfig',
    );
    expect(call.arguments, <String, Object?>{
      'playerId': 7,
      'maxQuantity': 80,
      'shadowStyle': 3,
      'customFontFamily': 'DanmakuRuntime_abc',
      'customFontFilePath': '/tmp/danmaku.otf',
      'blockWordsJson': '["spoiler","regex/[0-9]+/"]',
    });

    await player.dispose();
  });

  test('danmaku config coalesces rapid updates', () async {
    final player = ErikaPlayer();

    final first = player.setDanmakuConfig(fontSize: 24.0);
    final second = player.setDanmakuConfig(fontSize: 30.0);
    final third = player.setDanmakuConfig(fontSize: 30.0, opacity: 0.75);
    await Future.wait(<Future<void>>[first, second, third]);

    final calls = playerCalls
        .where((MethodCall call) => call.method == 'setDanmakuConfig')
        .toList(growable: false);
    expect(calls, hasLength(1));
    expect(calls.single.arguments, <String, Object?>{
      'playerId': 7,
      'fontSize': 30.0,
      'opacity': 0.75,
    });

    await player.setDanmakuConfig(fontSize: 30.0, opacity: 0.75);
    final callsAfterDuplicate = playerCalls
        .where((MethodCall call) => call.method == 'setDanmakuConfig')
        .toList(growable: false);
    expect(callsAfterDuplicate, hasLength(1));

    await player.dispose();
  });

  test('danmaku visibility bypasses config coalescing', () async {
    final player = ErikaPlayer();

    final update = player.setDanmakuConfig(enabled: false, opacity: 0.75);
    await Future<void>.delayed(Duration.zero);

    final visibilityCalls = playerCalls
        .where((MethodCall call) => call.method == 'setDanmakuEnabled')
        .toList(growable: false);
    expect(visibilityCalls, hasLength(1));
    expect(visibilityCalls.single.arguments, <String, Object?>{
      'playerId': 7,
      'enabled': false,
    });
    expect(
      playerCalls.where((MethodCall call) => call.method == 'setDanmakuConfig'),
      isEmpty,
    );

    await update;
    final configCall = playerCalls.singleWhere(
      (MethodCall call) => call.method == 'setDanmakuConfig',
    );
    expect(configCall.arguments, <String, Object?>{
      'playerId': 7,
      'opacity': 0.75,
    });

    await player.dispose();
  });

  test('danmaku track controls forward multi-track input', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'addDanmakuTrackFile' => 11,
        'addDanmakuTrackJson' => 12,
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final fileTrack = await player.addDanmakuTrackFile(
      '/tmp/a.xml',
      name: 'A',
      offset: const Duration(milliseconds: -500),
    );
    final jsonTrack = await player.addDanmakuTrackJson(
      '{"comments":[]}',
      name: 'B',
      offset: const Duration(milliseconds: 250),
    );
    await player.setDanmakuTrackEnabled(fileTrack, false);
    await player.setDanmakuTrackOffset(jsonTrack, const Duration(seconds: 1));
    await player.setDanmakuGlobalOffset(const Duration(milliseconds: -100));
    await player.removeDanmakuTrack(fileTrack);

    expect(fileTrack, 11);
    expect(jsonTrack, 12);
    expect(
      playerCalls
          .singleWhere(
            (MethodCall call) => call.method == 'addDanmakuTrackFile',
          )
          .arguments,
      <String, Object?>{
        'playerId': 7,
        'uri': '/tmp/a.xml',
        'name': 'A',
        'offsetMicros': -500000,
      },
    );
    expect(
      playerCalls
          .singleWhere(
            (MethodCall call) => call.method == 'addDanmakuTrackJson',
          )
          .arguments,
      <String, Object?>{
        'playerId': 7,
        'json': '{"comments":[]}',
        'name': 'B',
        'offsetMicros': 250000,
      },
    );
    expect(
      playerCalls
          .singleWhere(
            (MethodCall call) => call.method == 'setDanmakuTrackEnabled',
          )
          .arguments,
      <String, Object?>{'playerId': 7, 'trackId': 11, 'enabled': false},
    );
    expect(
      playerCalls
          .singleWhere(
            (MethodCall call) => call.method == 'setDanmakuTrackOffset',
          )
          .arguments,
      <String, Object?>{'playerId': 7, 'trackId': 12, 'offsetMicros': 1000000},
    );
    expect(
      playerCalls
          .singleWhere(
            (MethodCall call) => call.method == 'setDanmakuGlobalOffset',
          )
          .arguments,
      <String, Object?>{'playerId': 7, 'offsetMicros': -100000},
    );
    expect(
      playerCalls
          .singleWhere((MethodCall call) => call.method == 'removeDanmakuTrack')
          .arguments,
      <String, Object?>{'playerId': 7, 'trackId': 11},
    );

    await player.dispose();
  });

  test('danmaku tracks query parses native track list', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'danmakuTracks' => <Map<String, Object?>>[
            <String, Object?>{
              'id': 11,
              'enabled': true,
              'offsetMicros': -500000,
              'itemCount': 42,
              'name': 'A',
              'source': '/tmp/a.xml',
            },
          ],
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final tracks = await player.danmakuTracks();

    expect(tracks, hasLength(1));
    expect(tracks.single.id, 11);
    expect(tracks.single.enabled, isTrue);
    expect(tracks.single.offset, const Duration(milliseconds: -500));
    expect(tracks.single.itemCount, 42);
    expect(tracks.single.name, 'A');
    expect(tracks.single.source, '/tmp/a.xml');

    await player.dispose();
  });

  test('tracks query parses native track list', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      playerCalls.add(call);
      return switch (call.method) {
        'create' => 7,
        'tracks' => <Map<String, Object?>>[
            <String, Object?>{
              'id': 0,
              'kind': 0,
              'source': 0,
              'selected': true,
              'canRemove': false,
              'title': 'Main video',
              'language': null,
              'codec': 'hevc',
              'width': 3840,
              'height': 2160,
              'bitRate': 18000000,
              'frameRateNumerator': 30000,
              'frameRateDenominator': 1001,
            },
            <String, Object?>{
              'id': 1000001,
              'kind': 2,
              'source': 1,
              'selected': true,
              'canRemove': true,
              'title': 'subs.srt',
              'language': 'jpn',
              'codec': 'subrip',
            },
          ],
        'dispose' => null,
        _ => null,
      };
    });
    final player = ErikaPlayer();

    final tracks = await player.tracks();

    expect(tracks, hasLength(2));
    expect(tracks.first.kind, ErikaTrackKind.video);
    expect(tracks.first.source, ErikaTrackSource.embedded);
    expect(tracks.first.selected, isTrue);
    expect(tracks.first.codec, 'hevc');
    expect(tracks.first.width, 3840);
    expect(tracks.first.height, 2160);
    expect(tracks.first.bitRate, 18000000);
    expect(tracks.first.frameRateNumerator, 30000);
    expect(tracks.first.frameRateDenominator, 1001);
    expect(tracks.first.framesPerSecond, closeTo(29.970, 0.001));
    expect(tracks.last.kind, ErikaTrackKind.subtitle);
    expect(tracks.last.source, ErikaTrackSource.external);
    expect(tracks.last.canRemove, isTrue);
    expect(tracks.last.title, 'subs.srt');
    expect(tracks.last.bitRate, isNull);
    expect(tracks.last.framesPerSecond, isNull);

    await player.dispose();
  });

  test('player event parses track list and selection', () {
    final event = ErikaPlayerEvent.fromMap(<String, Object?>{
      'playerId': 7,
      'kind': ErikaEventKind.trackSelectionChanged.index,
      'state': ErikaPlaybackState.ready.index,
      'durationMicros': 0,
      'positionMicros': 0,
      'buffering': false,
      'video': <String, Object?>{},
      'tracks': <String, Object?>{'video': 1, 'audio': 1, 'subtitle': 1},
      'trackSelection': <String, Object?>{
        'video': 0,
        'audio': -1,
        'subtitle': 1000001,
      },
      'trackList': <Map<String, Object?>>[
        <String, Object?>{
          'id': 1000001,
          'kind': 2,
          'source': 1,
          'selected': true,
          'canRemove': true,
          'title': 'subs.ass',
          'language': null,
          'codec': 'ass',
        },
      ],
      'status': 0,
      'error': 'decoder failed',
      'message': 'hardware decoder fallback exhausted',
    });

    expect(event.kind, ErikaEventKind.trackSelectionChanged);
    expect(event.trackSelection.video, 0);
    expect(event.trackSelection.audio, isNull);
    expect(event.trackSelection.subtitle, 1000001);
    expect(event.trackList.single.isExternal, isTrue);
    expect(event.trackList.single.canRemove, isTrue);
    expect(event.error, 'decoder failed');
    expect(event.message, 'hardware decoder fallback exhausted');
  });

  test('presenter stats preserves AAudio recovery diagnostics', () {
    final stats = ErikaPresenterStats.fromMap(<String, Object?>{
      'audioRecoveryState': 3,
      'audioLastErrorCode': -899,
      'audioRecoveryAttempts': 4,
      'audioRecoveryCount': 2,
      'audioRecoveryFailures': 2,
      'videoFrameBackpressureDrops': 7,
    });

    expect(stats.audioRecoveryState, 3);
    expect(stats.audioLastErrorCode, -899);
    expect(stats.audioRecoveryAttempts, 4);
    expect(stats.audioRecoveryCount, 2);
    expect(stats.audioRecoveryFailures, 2);
    expect(stats.videoFrameBackpressureDrops, 7);
    expect(stats.toMap(), containsPair('audioRecoveryState', 3));
    expect(stats.toMap(), containsPair('audioLastErrorCode', -899));
    expect(stats.toMap(), containsPair('audioRecoveryAttempts', 4));
    expect(stats.toMap(), containsPair('audioRecoveryCount', 2));
    expect(stats.toMap(), containsPair('audioRecoveryFailures', 2));
    expect(stats.toMap(), containsPair('videoFrameBackpressureDrops', 7));
  });

  test('player event parses video decoder fallback details', () {
    final event = ErikaPlayerEvent.fromMap(<String, Object?>{
      'playerId': 9,
      'kind': ErikaEventKind.videoDecoderChanged.index,
      'state': ErikaPlaybackState.playing.index,
      'decoder': <String, Object?>{
        'stage': 'renderer_import',
        'requestedBackend': 'mediacodec',
        'previousBackend': 'mediacodec',
        'activeBackend': 'software',
        'fallbackCount': 1,
        'codec': 'h264',
        'pixelFormat': 'nv12',
        'lineSizes': <int>[1920, 1920, 0, 0],
        'reason': 'unsupported MediaCodec buffer layout',
      },
    });

    expect(event.kind, ErikaEventKind.videoDecoderChanged);
    expect(event.decoder?.stage, 'renderer_import');
    expect(event.decoder?.requestedBackend, 'mediacodec');
    expect(event.decoder?.activeBackend, 'software');
    expect(event.decoder?.fallbackCount, 1);
    expect(event.decoder?.lineSizes, <int>[1920, 1920, 0, 0]);
    expect(event.decoder?.reason, contains('buffer layout'));
  });

  test(
    'player event parses AAudio recovery details without kind downgrade',
    () {
      expect(ErikaEventKind.audioOutputChanged.index, 12);
      final event = ErikaPlayerEvent.fromMap(<String, Object?>{
        'playerId': 7,
        'kind': ErikaEventKind.audioOutputChanged.index,
        'state': ErikaPlaybackState.playing.index,
        'audio': <String, Object?>{
          'event': 'audio_output_changed',
          'recoveryState': 'stable',
          'lastErrorCode': -899,
          'recoveryAttempts': 2,
          'recoveryCount': 1,
          'recoveryFailures': 1,
          'transitionSequence': 5,
        },
        'message': '{"event":"audio_output_changed"}',
      });

      expect(event.kind, ErikaEventKind.audioOutputChanged);
      expect(event.audio?.recoveryState, 'stable');
      expect(event.audio?.lastErrorCode, -899);
      expect(event.audio?.recoveryAttempts, 2);
      expect(event.audio?.recoveryCount, 1);
      expect(event.audio?.recoveryFailures, 1);
      expect(event.audio?.transitionSequence, 5);
      expect(event.message, contains('audio_output_changed'));
    },
  );

  test('player event parses kind 13 system media navigation request', () {
    expect(ErikaEventKind.systemMediaNavigationRequested.index, 13);
    final nextEvent = ErikaPlayerEvent.fromMap(<String, Object?>{
      'playerId': 7,
      'kind': 13,
      'navigation': 'next',
    });
    final previousEvent = ErikaPlayerEvent.fromMap(<String, Object?>{
      'playerId': 7,
      'kind': 13.0,
      'navigation': 'previous',
    });
    final unknownEvent = ErikaPlayerEvent.fromMap(<String, Object?>{
      'playerId': 7,
      'kind': 13,
      'navigation': 'later',
    });

    expect(nextEvent.kind, ErikaEventKind.systemMediaNavigationRequested);
    expect(nextEvent.systemMediaCommand, ErikaSystemMediaCommand.next);
    expect(previousEvent.kind, ErikaEventKind.systemMediaNavigationRequested);
    expect(previousEvent.systemMediaCommand, ErikaSystemMediaCommand.previous);
    expect(unknownEvent.kind, ErikaEventKind.systemMediaNavigationRequested);
    expect(unknownEvent.systemMediaCommand, isNull);
  });
}
