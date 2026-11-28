import 'dart:io';
import 'dart:typed_data';

import 'package:erika_flutter/src/gif_export_native.dart';
import 'package:flutter_test/flutter_test.dart';

/// Runs the FFI frame export session (the path every platform but macOS
/// uses) against a real liberika library: set `ERIKA_FLUTTER_LIBRARY` to its
/// path, e.g. `target/release/liberika_capi.dylib`. Without it the tests are
/// skipped, as the package ships no native library of its own for tests.
void main() {
  final library = Platform.environment['ERIKA_FLUTTER_LIBRARY'];
  final skip = library == null || library.isEmpty
      ? 'set ERIKA_FLUTTER_LIBRARY to a liberika library to run this test'
      : false;
  final fixture = File('../../crates/erika/testdata/playback/playback-fixture.mkv');

  test('a session takes pictures one after another and survives a bad time',
      () async {
    if (!fixture.existsSync()) {
      markTestSkipped('the repository test media is not here');
      return;
    }
    final session = await NativeFrameExportSession.open(
      <String, Object?>{'uri': fixture.absolute.path},
    );
    try {
      final frames = <Map<String, Object?>>[];
      for (final millis in <int>[100, 200, 300, 2500]) {
        frames.add(await session.frame(<String, Object?>{
          'positionMillis': millis,
          'width': 64,
          'format': 0,
        }));
      }
      for (final frame in frames) {
        expect(frame['width'], 64);
        expect(frame['height'], 36);
        expect(frame['sourceWidth'], 160);
        expect(frame['durationMicros'], 8000000);
        expect(frame['toneMapped'], false);
        expect((frame['bytes']! as Uint8List).length, 64 * 36 * 4);
      }
      expect(frames.first['bytes'], isNot(frames.last['bytes']));
      await expectLater(
        session.frame(<String, Object?>{'positionMillis': 9000}),
        throwsA(isA<StateError>().having(
            (error) => error.message, 'message', contains('not before the end'))),
      );
      final png = await session.frame(<String, Object?>{
        'positionMillis': 100,
        'format': 1,
      });
      expect((png['bytes']! as Uint8List).sublist(0, 4), <int>[0x89, 0x50, 0x4e, 0x47]);
    } finally {
      await session.close();
    }
    await expectLater(
      session.frame(<String, Object?>{'positionMillis': 0}),
      throwsA(isA<StateError>()),
    );
  }, skip: skip);

  test('opening a media that is not there throws liberika\'s message',
      () async {
    await expectLater(
      NativeFrameExportSession.open(
          <String, Object?>{'uri': '/nonexistent/erika-missing.mkv'}),
      throwsA(isA<StateError>()),
    );
  }, skip: skip);

  test('the single export reports the picture too', () async {
    if (!fixture.existsSync()) {
      markTestSkipped('the repository test media is not here');
      return;
    }
    final frame = await exportFrameNative(<String, Object?>{
      'uri': fixture.absolute.path,
      'positionMillis': 1000,
      'width': 80,
      'format': 1,
    });
    expect(frame['width'], 80);
    expect(frame['toneMapped'], false);
  }, skip: skip);
}
