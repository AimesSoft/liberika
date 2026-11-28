import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';

final class ErikaGifExportOptions extends Struct {
  external Pointer<Int8> inputUri;
  external Pointer<Int8> outputPath;
  @Uint64()
  external int startMillis;
  @Uint64()
  external int endMillis;
  @Uint32()
  external int framesPerSecond;
  @Uint32()
  external int outputWidth;
  @Uint32()
  external int outputHeight;
  @Int32()
  external int quality;
  @Int32()
  external int loopCount;
  @Bool()
  external bool overwrite;
  external Pointer<Void> headers;
  @UintPtr()
  external int headerCount;
  @Uint64()
  external int httpReadAheadBytes;
  @Array(3)
  external Array<Uint64> reserved;
}

final class ErikaGifExportResult extends Struct {
  @Uint32()
  external int width;
  @Uint32()
  external int height;
  @Uint64()
  external int frameCount;
  @Uint64()
  external int fileSize;
}

typedef _MallocNative = Pointer<Void> Function(IntPtr);
typedef _MallocDart = Pointer<Void> Function(int);
typedef _FreeNative = Void Function(Pointer<Void>);
typedef _FreeDart = void Function(Pointer<Void>);
typedef _ExportNative =
    Int32 Function(
      Pointer<ErikaGifExportOptions>,
      Pointer<ErikaGifExportResult>,
    );
typedef _ExportDart =
    int Function(Pointer<ErikaGifExportOptions>, Pointer<ErikaGifExportResult>);
typedef _LastErrorNative = Pointer<Int8> Function();
typedef _LastErrorDart = Pointer<Int8> Function();
typedef _StringFreeNative = Void Function(Pointer<Int8>);
typedef _StringFreeDart = void Function(Pointer<Int8>);

void main(List<String> arguments) async {
  if (arguments.length < 3 || arguments.length > 5) {
    stderr.writeln(
      'usage: dart run scripts/gif_export_smoke.dart '
      '<liberika_capi.dylib> <input.mp4> <output.gif> '
      '[start-seconds] [frames-per-second]',
    );
    exitCode = 64;
    return;
  }
  final libraryPath = File(arguments[0]).absolute.path;
  final inputArgument = arguments[1];
  final inputUri =
      inputArgument.startsWith('http://') ||
          inputArgument.startsWith('https://')
      ? inputArgument
      : File(inputArgument).absolute.path;
  final outputPath = File(arguments[2]).absolute.path;
  final startMillis =
      ((double.tryParse(arguments.length >= 4 ? arguments[3] : '0') ?? 0) *
              1000)
          .round();
  final framesPerSecond =
      int.tryParse(arguments.length >= 5 ? arguments[4] : '10') ?? 10;
  final result = await Isolate.run(
    () => _runExport(
      libraryPath,
      inputUri,
      outputPath,
      startMillis,
      framesPerSecond,
    ),
  );
  stdout.writeln(jsonEncode(result));
}

Map<String, Object> _runExport(
  String libraryPath,
  String inputUri,
  String outputPath,
  int startMillis,
  int framesPerSecond,
) {
  final process = DynamicLibrary.process();
  final malloc = process.lookupFunction<_MallocNative, _MallocDart>('malloc');
  final free = process.lookupFunction<_FreeNative, _FreeDart>('free');
  final library = DynamicLibrary.open(libraryPath);
  final exportGif = library.lookupFunction<_ExportNative, _ExportDart>(
    'erika_export_gif',
  );
  final lastError = library.lookupFunction<_LastErrorNative, _LastErrorDart>(
    'erika_last_error_message',
  );
  final stringFree = library.lookupFunction<_StringFreeNative, _StringFreeDart>(
    'erika_string_free',
  );

  final input = _nativeUtf8(inputUri, malloc);
  final output = _nativeUtf8(outputPath, malloc);
  final options = malloc(
    sizeOf<ErikaGifExportOptions>(),
  ).cast<ErikaGifExportOptions>();
  final result = malloc(
    sizeOf<ErikaGifExportResult>(),
  ).cast<ErikaGifExportResult>();
  try {
    options.ref
      ..inputUri = input
      ..outputPath = output
      ..startMillis = startMillis
      ..endMillis = startMillis + 5000
      ..framesPerSecond = framesPerSecond
      ..outputWidth = 640
      ..outputHeight = 360
      ..quality = 1
      ..loopCount = 0
      ..overwrite = false
      ..headers = nullptr
      ..headerCount = 0
      ..httpReadAheadBytes = 0;
    for (var index = 0; index < 3; index++) {
      options.ref.reserved[index] = 0;
    }
    final status = exportGif(options, result);
    if (status != 0) {
      final errorPointer = lastError();
      final message = errorPointer == nullptr
          ? 'unknown Erika error'
          : _readUtf8(errorPointer);
      if (errorPointer != nullptr) {
        stringFree(errorPointer);
      }
      throw StateError('erika_export_gif failed ($status): $message');
    }
    return <String, Object>{
      'outputPath': outputPath,
      'width': result.ref.width,
      'height': result.ref.height,
      'frameCount': result.ref.frameCount,
      'fileSize': result.ref.fileSize,
      'startMillis': startMillis,
      'endMillis': startMillis + 5000,
      'framesPerSecond': framesPerSecond,
      'quality': 'high',
    };
  } finally {
    free(input.cast());
    free(output.cast());
    free(options.cast());
    free(result.cast());
  }
}

Pointer<Int8> _nativeUtf8(String value, _MallocDart malloc) {
  final bytes = utf8.encode(value);
  final pointer = malloc(bytes.length + 1).cast<Uint8>();
  pointer.asTypedList(bytes.length + 1)
    ..setRange(0, bytes.length, bytes)
    ..[bytes.length] = 0;
  return pointer.cast();
}

String _readUtf8(Pointer<Int8> pointer) {
  var length = 0;
  while (pointer[length] != 0) {
    length++;
  }
  return utf8.decode(pointer.cast<Uint8>().asTypedList(length));
}
