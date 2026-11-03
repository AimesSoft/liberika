import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

/// The macOS and iOS plugins never run native work on the main thread: a
/// presenter's JSON pass-through and diagnostics run on its owner thread, the
/// process-wide logs on a utility queue, and only the result comes back to the
/// platform thread, which is the app's interface.
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

      test(
          'answers invokeJson and getDiagnosticsSnapshot from the presenter '
          'owner and getLogs from a utility queue', () {
        for (final method in <String>['invokeJson', 'getDiagnosticsSnapshot']) {
          final body = handler(method);
          expect(body, contains('host.enqueue('), reason: method);
          expect(body, isNot(contains('result(try')), reason: method);
        }
        final logs = handler('getLogs');
        expect(logs, contains('respondOffMain(queue: logQueue'));
        expect(logs, isNot(contains('result(try')));
        expect(plugin, contains('DispatchQueue.main.async {'));
      });

      test('methods without a presenter run without the native call lock', () {
        expect(plugin, contains('library.isGlobalJsonMethod(method)'));
        expect(plugin, contains('? call(nil)'));
      });

      test('main-thread timers hand native polling to the owner', () {
        final start = plugin.indexOf('  func pollEvents(sendEvent:');
        expect(start, isNonNegative);
        final body = plugin.substring(start, plugin.indexOf('\n  }\n', start));
        expect(body, contains('if !owner.isCurrent {'));
        expect(body, contains('owner.requestMaintenance {'));
        expect(plugin, isNot(contains('nativeCallLock')));
      });
    });
  }

  test('macOS exposes the frame export session to Dart', () {
    final plugin =
        File('macos/Classes/ErikaFlutterPlugin.swift').readAsStringSync();
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
