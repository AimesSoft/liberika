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

      // Stats are read on the presenter's owner, where the render tick
      // stores them, so no lock or main-thread wait is involved.
      expect(
        plugin,
        contains(
          'func presenterStats() -> [String: Any] {\n'
          '    owner.assertCurrent()\n'
          '    return latestPresenterStats.toFlutterMap()',
        ),
      );
    });
  }

  test('Apple platforms ship the same presenter owner', () {
    final canonical =
        File('macos/Classes/ErikaPresenterOwner.swift').readAsStringSync();
    for (final platform in <String>['ios', 'tvos']) {
      expect(
        File('$platform/Classes/ErikaPresenterOwner.swift').readAsStringSync(),
        canonical,
        reason: platform,
      );
    }
  });

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
