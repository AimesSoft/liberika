import 'dart:convert';
import 'dart:io';

import 'package:erika_flutter/erika_flutter.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  const playerChannel = MethodChannel('erika_flutter/player');
  const eventsChannel = MethodChannel('erika_flutter/events');
  late List<MethodCall> calls;

  void mockPlayer(Object? Function(MethodCall call) handler) {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, (MethodCall call) async {
      calls.add(call);
      if (call.method == 'create') {
        return 7;
      }
      if (call.method == 'dispose') {
        return null;
      }
      return handler(call);
    });
  }

  setUp(() {
    calls = <MethodCall>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(
            eventsChannel, (MethodCall call) async => null);
  });

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(playerChannel, null);
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(eventsChannel, null);
  });

  group('getDiagnosticsSnapshot', () {
    test('decodes the JSON string returned by Apple plugins', () async {
      mockPlayer((MethodCall call) => jsonEncode(<String, Object?>{
            'schema': 1,
            'decoder': <String, Object?>{
              'activeBackend': 'videotoolbox',
              'hardware': true
            },
            'timing': <String, Object?>{
              'scheduler': <String, Object?>{
                'lateDroppedFrames': 3,
                'maxDroppedLateMs': null
              },
            },
          }));
      final player = ErikaPlayer();
      final snapshot = await player.getDiagnosticsSnapshot();

      expect(snapshot['schema'], 1);
      expect((snapshot['decoder'] as Map<String, Object?>)['hardware'], isTrue);
      final timing = snapshot['timing'] as Map<String, Object?>;
      final scheduler = timing['scheduler'] as Map<String, Object?>;
      expect(scheduler['lateDroppedFrames'], 3);
      expect(scheduler.containsKey('maxDroppedLateMs'), isTrue);
      expect(scheduler['maxDroppedLateMs'], isNull);
      expect(
        calls
            .singleWhere(
                (MethodCall call) => call.method == 'getDiagnosticsSnapshot')
            .arguments,
        <String, Object?>{'playerId': 7},
      );
      await player.dispose();
    });

    test('accepts the decoded map returned by JSON-bridge platforms', () async {
      mockPlayer((MethodCall call) => <Object?, Object?>{
            'schema': 1,
            'buffers': <Object?, Object?>{
              'playback': null,
              'presenterVideoFrames': <Object?, Object?>{
                'queued': 1,
                'capacity': 8
              },
            },
            'media': <Object?, Object?>{'video': null},
          });
      final player = ErikaPlayer();
      final snapshot = await player.getDiagnosticsSnapshot();
      final buffers = snapshot['buffers'] as Map<String, Object?>;
      expect(buffers['playback'], isNull);
      expect(
          (buffers['presenterVideoFrames'] as Map<String, Object?>)['capacity'],
          8);
      await player.dispose();
    });

    test('a missing platform implementation is an UnsupportedError', () async {
      mockPlayer((MethodCall call) => throw MissingPluginException());
      final player = ErikaPlayer();
      await expectLater(
          player.getDiagnosticsSnapshot(), throwsUnsupportedError);
      await player.dispose();
    });

    test('a native library without the symbol is an UnsupportedError',
        () async {
      mockPlayer((MethodCall call) => throw PlatformException(
            code: 'ERIKA_UNSUPPORTED',
            message:
                'The loaded Erika native library does not export erika_presenter_get_diagnostics_json.',
          ));
      final player = ErikaPlayer();
      await expectLater(
          player.getDiagnosticsSnapshot(), throwsUnsupportedError);
      await player.dispose();
    });

    test('an older JSON bridge without the method is an UnsupportedError',
        () async {
      mockPlayer((MethodCall call) => throw PlatformException(
            code: 'erika_linux',
            message:
                'unsupported Erika presenter method: getDiagnosticsSnapshot',
          ));
      final player = ErikaPlayer();
      await expectLater(
          player.getDiagnosticsSnapshot(), throwsUnsupportedError);
      await player.dispose();
    });

    test('other native failures are rethrown unchanged', () async {
      mockPlayer((MethodCall call) =>
          throw PlatformException(code: 'ERIKA_ERROR', message: 'boom'));
      final player = ErikaPlayer();
      await expectLater(
        player.getDiagnosticsSnapshot(),
        throwsA(isA<PlatformException>()
            .having((e) => e.message, 'message', 'boom')),
      );
      await player.dispose();
    });
  });

  group('getLogs', () {
    test('forwards limit, level and sequence and decodes entries', () async {
      mockPlayer((MethodCall call) => jsonEncode(<String, Object?>{
            'schema': 1,
            'nextSeq': 42,
            'redacted': true,
            'entries': <Object?>[
              <String, Object?>{
                'seq': 41,
                'level': 'warn',
                'source': 'ffmpeg',
                'message': '[hevc @ 0x1] Could not find ref with POC 12',
                'count': 3,
              },
            ],
          }));
      final player = ErikaPlayer();
      final logs =
          await player.getLogs(limit: 50, minLevel: 'warn', sinceSeq: 40);

      expect(logs['nextSeq'], 42);
      final entries = logs['entries'] as List<Object?>;
      expect((entries.single as Map<String, Object?>)['source'], 'ffmpeg');
      expect(
        calls
            .singleWhere((MethodCall call) => call.method == 'getLogs')
            .arguments,
        <String, Object?>{
          'playerId': 7,
          'limit': 50,
          'minLevel': 'warn',
          'sinceSeq': 40
        },
      );
      await player.dispose();
    });

    test('omits optional filters and defaults to the newest 200', () async {
      mockPlayer((MethodCall call) =>
          <Object?, Object?>{'schema': 1, 'entries': <Object?>[]});
      final player = ErikaPlayer();
      await player.getLogs();
      expect(
        calls
            .singleWhere((MethodCall call) => call.method == 'getLogs')
            .arguments,
        <String, Object?>{'playerId': 7, 'limit': 200},
      );
      await player.dispose();
    });

    test('validates arguments before any channel call', () async {
      mockPlayer((MethodCall call) => null);
      final player = ErikaPlayer();
      expect(() => player.getLogs(limit: -1), throwsArgumentError);
      expect(() => player.getLogs(minLevel: 'loud'), throwsArgumentError);
      expect(() => player.getLogs(sinceSeq: -5), throwsArgumentError);
      expect(
          calls.where((MethodCall call) => call.method == 'getLogs'), isEmpty);
      expect(ErikaPlayer.logLevels,
          <String>['trace', 'debug', 'info', 'warn', 'error']);
      await player.dispose();
    });
  });

  group('invokeJson', () {
    test('returns the native envelope from Apple plugins unchanged', () async {
      mockPlayer((MethodCall call) => jsonEncode(<String, Object?>{
            'ok': false,
            'status': 3,
            'code': 'unknown_method',
            'method': 'warpDrive',
            'error': 'unsupported Erika presenter method: warpDrive',
          }));
      final player = ErikaPlayer();
      final response =
          await player.invokeJson('warpDrive', <String, Object?>{'speed': 9});

      expect(response['ok'], isFalse);
      expect(response['code'], 'unknown_method');
      expect(
        calls
            .singleWhere((MethodCall call) => call.method == 'invokeJson')
            .arguments,
        <String, Object?>{
          'playerId': 7,
          'method': 'warpDrive',
          'arguments': <String, Object?>{'speed': 9},
        },
      );
      await player.dispose();
    });

    test('accepts the inner envelope map from JSON-bridge platforms', () async {
      mockPlayer((MethodCall call) => <Object?, Object?>{
            'ok': true,
            'status': 0,
            'value': <Object?, Object?>{
              'schema': 1,
              'methods': <Object?>[
                <Object?, Object?>{'name': 'listMethods', 'scope': 'global'},
              ],
            },
          });
      final player = ErikaPlayer();
      final response = await player.invokeJson('listMethods');
      final value = response['value'] as Map<String, Object?>;
      final methods = value['methods'] as List<Object?>;
      expect((methods.single as Map<String, Object?>)['scope'], 'global');
      expect(
        (calls
            .singleWhere((MethodCall call) => call.method == 'invokeJson')
            .arguments as Map<Object?, Object?>)['arguments'],
        <String, Object?>{},
      );
      await player.dispose();
    });

    test('pass-through failures surface as an error envelope', () async {
      mockPlayer((MethodCall call) => throw PlatformException(
            code: 'ERIKA_ERROR',
            message: 'method is required',
          ));
      final player = ErikaPlayer();
      final response = await player.invokeJson('getLogs');
      expect(response, <String, Object?>{
        'ok': false,
        'status': 3,
        'error': 'method is required'
      });
      await player.dispose();
    });

    test('a platform without the pass-through is an UnsupportedError',
        () async {
      mockPlayer((MethodCall call) => throw MissingPluginException());
      final player = ErikaPlayer();
      await expectLater(
          player.invokeJson('listMethods'), throwsUnsupportedError);
      expect(() => player.invokeJson(''), throwsArgumentError);
      await player.dispose();
    });
  });

  group('platform contracts', () {
    for (final platform in <String>['macos', 'ios', 'tvos']) {
      test('$platform resolves diagnostics symbols optionally', () {
        final plugin = File('$platform/Classes/ErikaFlutterPlugin.swift')
            .readAsStringSync();
        for (final symbol in <String>[
          'erika_presenter_get_diagnostics_json',
          'erika_log_read_json',
          'erika_presenter_invoke_json',
        ]) {
          expect(plugin, contains('Self.loadOptional("$symbol"'));
          expect(plugin, isNot(contains('try Self.load("$symbol"')));
        }
        for (final method in <String>[
          'getDiagnosticsSnapshot',
          'getLogs',
          'invokeJson'
        ]) {
          expect(plugin, contains('case "$method":'));
        }
        expect(plugin, contains('"ERIKA_UNSUPPORTED"'));
      });
    }

    test('Android and HarmonyOS forward the methods to the shared JSON table',
        () {
      final android = File(
        'android/src/main/kotlin/dev/aimesoft/erika_flutter/ErikaFlutterPlugin.kt',
      ).readAsStringSync();
      final ohos = File(
        'ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets',
      ).readAsStringSync();
      for (final method in <String>[
        'getDiagnosticsSnapshot',
        'getLogs',
        'invokeJson'
      ]) {
        expect(android, contains('"$method",'));
        expect(ohos, contains("'$method'"));
      }
    });

    test('the bundled header declares the diagnostics functions', () {
      final header = File('native/include/erika.h').readAsStringSync();
      // The repository header is absent when the package is checked alone.
      final publicHeader = File('../../crates/erika_capi/include/erika.h');
      if (publicHeader.existsSync()) {
        expect(header, publicHeader.readAsStringSync());
      }
      expect(header, contains('ErikaStatus erika_log_read_json('));
      expect(header,
          contains('ErikaStatus erika_presenter_get_diagnostics_json('));
      expect(header, contains('"listMethods"'));
    });
  });
}
