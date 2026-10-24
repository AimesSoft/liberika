import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

/// iOS and tvOS link liberika statically and look its functions up in the
/// running executable (`dlsym` from the Swift plugin, `DynamicLibrary.process()`
/// from the Dart exports). The linker drops what nothing references, so every
/// function those lookups need is named in the podspec's `-u` list, except
/// the ones newer than the pinned native artifacts: forcing a symbol the
/// static library lacks fails the link, so those are listed as optional (the
/// plugin and the Dart side look them up optionally) until the release whose
/// artifacts ship them moves them into the forced list.
void main() {
  final header = File('native/include/erika.h').readAsStringSync();
  final declared = RegExp(r'\b(erika_\w+)\s*\(')
      .allMatches(header.replaceAll(RegExp(r'/\*.*?\*/', dotAll: true), ''))
      .map((match) => match.group(1)!)
      .toSet();

  Set<String> listed(String podspec, String name) {
    final match = RegExp('$name = %w\\[(.*?)\\]', dotAll: true)
        .firstMatch(podspec);
    if (match == null) return {};
    return match
        .group(1)!
        .split(RegExp(r'\s+'))
        .where((name) => name.isNotEmpty)
        .toSet();
  }

  for (final platform in <String>['ios', 'tvos']) {
    test('$platform names every export and diagnostics function', () {
      final podspec = File('$platform/erika_flutter.podspec').readAsStringSync();
      final forced = listed(podspec, 'erika_cabi_symbols');
      final optional = listed(podspec, 'erika_cabi_optional_symbols');
      // In the pinned 0.2.1 artifacts: forced.
      for (final symbol in <String>[
        'erika_export_gif',
        'erika_last_error_message',
        'erika_string_free',
        'erika_presenter_invoke_json',
      ]) {
        expect(forced, contains(symbol));
      }
      // Newer than the pinned artifacts: named, forced or optional.
      for (final symbol in <String>[
        'erika_export_frame',
        'erika_frame_export_result_free',
        'erika_frame_export_session_open',
        'erika_frame_export_session_frame',
        'erika_frame_export_session_close',
        'erika_presenter_get_diagnostics_json',
        'erika_log_read_json',
      ]) {
        expect(forced.union(optional), contains(symbol));
      }
      expect(forced.intersection(optional), isEmpty);
      // A name that liberika does not define would fail the final link.
      expect(declared.containsAll(forced), isTrue,
          reason: 'not in erika.h: ${forced.difference(declared)}');
    });
  }
}
