import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

void main() {
  test('automatic output mode keeps the native ABI value 3', () {
    final header = File(
      'native/include/erika.h',
    ).readAsStringSync();
    final dart = File('lib/src/erika_player.dart').readAsStringSync();

    expect(header, contains('ErikaPresenterOutputMode_Auto = 3'));
    expect(dart, contains('auto(3)'));
    expect(dart, contains('3 => ErikaOutputMode.auto'));
  });

  for (final platform in <String>['macos', 'ios', 'tvos']) {
    test('$platform defaults Apple output to source-aware auto mode', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(
        plugin,
        anyOf(
          contains(
              'let config = ErikaPresenterConfigC.auto(headroom: headroom)'),
          contains('return .auto(headroom: headroom)'),
        ),
      );
      expect(plugin, contains('case 3:'));
      expect(plugin, contains('getResourceStatus'));
      expect(plugin, contains('erika_presenter_get_resource_status'));
    });
  }

  for (final platform in <String>['ios', 'tvos']) {
    test('$platform does not reset active auto output during resize', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(plugin, contains('if attach || presenterConfig.outputMode != 3'));
    });

    test('$platform presenter stats returns its Flutter map', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      // iOS does not wait for the player's lock on the main thread while a
      // long call on another queue holds it: it returns the last stats.
      expect(
        plugin,
        contains(
          platform == 'ios'
              ? 'func presenterStats() -> [String: Any] {\n'
                  '    // The last stats the render tick stored; while another queue is inside a\n'
                  '    // long call the previous copy is returned instead of waiting for it.\n'
                  '    tryWithNativeCall { latestPresenterStats.toFlutterMap() }\n'
                  '      ?? latestPresenterStats.toFlutterMap()'
              : 'func presenterStats() -> [String: Any] {\n'
                  '    nativeCallLock.lock()\n'
                  '    defer { nativeCallLock.unlock() }\n'
                  '    return latestPresenterStats.toFlutterMap()',
        ),
      );
    });
  }

  test('Metal limits contentsFormat switching to UIKit and tvOS', () {
    final rendererFile = File('../../crates/erika/src/renderer/metal/apple.rs');
    if (!rendererFile.existsSync()) {
      return;
    }
    final renderer = rendererFile.readAsStringSync();

    expect(renderer, contains('layer.setPixelFormat('));
    expect(
      renderer,
      contains(
        '#[cfg(any(target_os = "ios", target_os = "tvos"))]\n'
        '    {\n'
        '        let contents_format',
      ),
    );
    expect(renderer, contains('layer.setContentsFormat(contents_format)'));
    expect(renderer, contains('kCAContentsFormatRGBA16Float'));
    expect(renderer, contains('kCAContentsFormatRGBA8Uint'));
    expect(renderer, contains('clips the right/bottom at 2x'));
  });
}
