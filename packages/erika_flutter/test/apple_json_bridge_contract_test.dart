import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

/// The macOS and iOS plugins run the JSON pass-through, the diagnostics
/// snapshot and the logs off the main thread: they take as long as the work
/// they ask for (a subtitle file parsed, a picture decoded), and on the
/// platform thread that is the app's interface.
void main() {
  for (final platform in <String>['macos', 'ios']) {
    group(platform, () {
      late String plugin;
      setUpAll(() {
        plugin = File('$platform/Classes/ErikaFlutterPlugin.swift')
            .readAsStringSync();
      });

      String handler(String method) {
        final start = plugin.indexOf('      case "$method":');
        expect(start, isNonNegative, reason: 'no handler for $method');
        final next = plugin.indexOf('\n      case "', start + 1);
        return plugin.substring(start, next);
      }

      test('answers invokeJson, getDiagnosticsSnapshot and getLogs after a '
          'background queue did the work', () {
        for (final method in <String>[
          'invokeJson',
          'getDiagnosticsSnapshot',
          'getLogs',
        ]) {
          final body = handler(method);
          expect(body, contains('respondOffMain('), reason: method);
          expect(body, isNot(contains('result(try')), reason: method);
        }
        expect(plugin, contains('DispatchQueue.main.async {'));
        expect(plugin, contains('let jsonQueue: DispatchQueue'));
      });

      test('methods without a presenter run without the native call lock', () {
        expect(plugin, contains('library.isGlobalJsonMethod(method)'));
        expect(plugin, contains('? call(nil)'));
      });

      test('main-thread timers skip a tick instead of waiting for the lock',
          () {
        expect(plugin, contains('_ = tryWithNativeCall {'));
        expect(plugin, contains('guard nativeCallLock.try() else { return nil }'));
      });
    });
  }

  test('macOS exposes the frame export session to Dart', () {
    final plugin = File('macos/Classes/ErikaFlutterPlugin.swift')
        .readAsStringSync();
    for (final method in <String>[
      'openFrameExportSession',
      'exportSessionFrame',
      'closeFrameExportSession',
    ]) {
      expect(plugin, contains('case "$method"'));
    }
    for (final symbol in <String>[
      'erika_frame_export_session_open',
      'erika_frame_export_session_frame',
      'erika_frame_export_session_close',
    ]) {
      expect(plugin, contains('"$symbol"'));
    }
  });
}
