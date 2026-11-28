import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

void main() {
  test('OpenHarmony screenshot returns the composited RGBA frame', () {
    final plugin = File(
      'ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets',
    ).readAsStringSync();
    // Every presenter operation runs on the presenter's owner thread; the
    // bridge exports each one through a single dispatch table, with an
    // `Async` variant that resolves a promise on the ArkTS thread.
    final nativeBridge = File(
      'ohos/src/main/cpp/presenter_bridge.h',
    ).readAsStringSync();

    expect(plugin, contains('this.captureFrame(args, result);'));
    expect(plugin, contains('erikaNative.nativeCaptureFrameAsync('));
    expect(plugin, contains('as Uint8Array | null'));

    expect(
      nativeBridge,
      contains('erika_presenter_capture_frame_rgba('),
    );
    expect(nativeBridge, contains('napi_create_arraybuffer('));
    expect(nativeBridge, contains('napi_create_typedarray('));
    expect(nativeBridge, contains('{"nativeCaptureFrame", Op::Capture}'));
  });

  test('OpenHarmony exposes the complete subtitle memory font API', () {
    final plugin = File(
      'ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets',
    ).readAsStringSync();
    final nativeBridge = File(
      'ohos/src/main/cpp/presenter_bridge.h',
    ).readAsStringSync();

    expect(plugin, contains("call.method === 'registerSubtitleMemoryFont'"));
    expect(plugin, contains('nativeRegisterSubtitleMemoryFontAsync('));
    for (final method in <String>[
      'selectSubtitleMemoryFonts',
      'clearSubtitleMemoryFonts',
      'getSubtitleMemoryFontStatus',
    ]) {
      expect(plugin, contains("'$method'"));
    }
    expect(
      nativeBridge,
      contains('erika_presenter_register_subtitle_memory_font('),
    );
    expect(
      nativeBridge,
      contains('{"nativeRegisterSubtitleMemoryFont", Op::Font}'),
    );
  });

  test('OpenHarmony can consume and package the prebuilt runtime', () {
    final cmake = File(
      'ohos/src/main/cpp/CMakeLists.txt',
    ).readAsStringSync();

    expect(cmake, contains(r'$ENV{ERIKA_FORCE_SOURCE_BUILD}'));
    expect(cmake, contains(r'$ENV{ERIKA_PREBUILT_TAG}'));
    expect(cmake, contains('EXPECTED_HASH'));
    expect(
      cmake,
      contains('erika-capi-openharmony-arm64.zip'),
    );
    expect(cmake, contains('ERIKA_USE_PREBUILT'));
    expect(cmake, contains('ERIKA_PACKAGE_ROOT}/native/include'));
    expect(cmake, contains('copy_if_different'));
    expect(
      cmake,
      contains(r'$<TARGET_FILE_DIR:${PLUGIN_NAME}>/liberika_capi.so'),
    );
  });

  test('OpenHarmony media controls avoid implicit ESObject types', () {
    final plugin = File(
      'ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets',
    ).readAsStringSync();

    for (final declaration in <String>[
      "const metadataValue: ESObject = args.get('metadata');",
      'const nativeArgs: Map<string, ESObject> =',
      'const metadata: Map<string, ESObject> =',
      "const titleValue: ESObject = metadata.get('title');",
      "const artist: ESObject = metadata.get('artist');",
      "const album: ESObject = metadata.get('album');",
      "const artwork: ESObject = metadata.get('artwork');",
      'const args: Map<string, ESObject> =',
    ]) {
      expect(plugin, contains(declaration));
    }
  });
}
