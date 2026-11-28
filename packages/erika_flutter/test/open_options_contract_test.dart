import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

void main() {
  test('Windows accepts both StandardMessageCodec integer widths', () {
    final plugin = File(
      'windows/erika_flutter_plugin.cpp',
    ).readAsStringSync();
    final parseStart = plugin.indexOf('uint64_t OpenByteCountArg(');
    final parseEnd = plugin.indexOf('std::optional<int64_t> Int64Value(', parseStart);

    expect(parseStart, greaterThanOrEqualTo(0));
    expect(parseEnd, greaterThan(parseStart));
    final parser = plugin.substring(parseStart, parseEnd);
    expect(parser, contains('std::get_if<int32_t>'));
    expect(parser, contains('std::get_if<int64_t>'));
    expect(parser, contains('*value < 0'));

    // Both HTTP byte-count arguments share the one validated parser.
    for (final name in <String>['httpReadAheadBytes', 'httpBackBufferBytes']) {
      expect(plugin, contains('OpenByteCountArg(args, "$name")'));
    }
  });

  for (final platform in <String>['ios', 'macos', 'tvos']) {
    test('$platform validates read-ahead before converting to UInt64', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(
        plugin,
        contains(
          'let readAhead = try optionalReadAheadBytes('
          'args["httpReadAheadBytes"])',
        ),
      );
      expect(plugin, contains('numericValue >= 0'));
      expect(plugin,
          contains('numericValue.rounded(.towardZero) == numericValue'));
    });

    test('$platform validates the rewind budget through the same helper', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(
        plugin,
        contains(
          'let backBuffer = try optionalBackBufferBytes('
          'args["httpBackBufferBytes"])',
        ),
      );
      expect(
        plugin,
        contains(
          'try optionalByteCount(value, name: "httpBackBufferBytes")',
        ),
      );
    });
  }
}
