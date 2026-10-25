import AppKit
import CoreVideo
import Darwin
import FlutterMacOS
import Metal
import MediaPlayer
import ObjectiveC.runtime
import QuartzCore

private let erikaWindowHostedVideoSurfaceId: Int64 = -1
private let erikaDebugLabelsEnabled =
  ProcessInfo.processInfo.environment["ERIKA_DEBUG_LABELS"] == "1"
private let erikaWindowOverlayTraceEnabled =
  ProcessInfo.processInfo.environment["ERIKA_WINDOW_OVERLAY_TRACE"] == "1"
private let erikaDefaultDisplayFps = 60.0
private let erikaDisplayFpsEpsilon = 0.5

private func erikaWindowOverlayTrace(_ message: @autoclosure () -> String) {
  guard erikaWindowOverlayTraceEnabled else {
    return
  }
  NSLog("[ErikaWindowOverlay] %@", message())
}

/// Drives rendering from the display's vertical refresh instead of a main-run-loop
/// `Timer`. `CVDisplayLink` already owns a dedicated high-priority callback
/// thread, so render directly there instead of waking a second GCD worker for
/// every frame. The callback stays coalesced while a render is in progress so a
/// slow `CAMetalLayer.nextDrawable()` cannot build up callbacks and replay them.
private final class ErikaDisplayLinkDriver {
  private let onTick: () -> Void
  private let stateLock = NSLock()
  private var displayLink: CVDisplayLink?
  private var tickQueued = false
  private var invalidated = false

  init?(displayID: CGDirectDisplayID, onTick: @escaping () -> Void) {
    self.onTick = onTick

    var link: CVDisplayLink?
    guard CVDisplayLinkCreateWithCGDisplay(displayID, &link) == kCVReturnSuccess,
          let link else {
      return nil
    }
    displayLink = link

    let status = CVDisplayLinkSetOutputCallback(
      link,
      { _, _, _, _, _, context in
        guard let context else {
          return kCVReturnError
        }
        let driver = Unmanaged<ErikaDisplayLinkDriver>
          .fromOpaque(context)
          .takeUnretainedValue()
        driver.displayDidRefresh()
        return kCVReturnSuccess
      },
      Unmanaged.passUnretained(self).toOpaque()
    )
    guard status == kCVReturnSuccess else {
      displayLink = nil
      return nil
    }
  }

  deinit {
    invalidate()
  }

  func start() -> Bool {
    guard let displayLink else {
      return false
    }
    return CVDisplayLinkStart(displayLink) == kCVReturnSuccess
  }

  func invalidate() {
    stateLock.lock()
    invalidated = true
    stateLock.unlock()

    if let displayLink, CVDisplayLinkIsRunning(displayLink) {
      CVDisplayLinkStop(displayLink)
    }
    displayLink = nil
  }

  private func displayDidRefresh() {
    stateLock.lock()
    guard !invalidated, !tickQueued else {
      stateLock.unlock()
      return
    }
    tickQueued = true
    stateLock.unlock()

    stateLock.lock()
    let shouldRender = !invalidated
    stateLock.unlock()

    if shouldRender {
      onTick()
    }

    // Keep tickQueued set throughout onTick(). Any display refresh received
    // while Metal is blocked is deliberately dropped rather than replayed.
    stateLock.lock()
    tickQueued = false
    stateLock.unlock()
  }
}

/// A Flutter texture whose backing CVPixelBuffer is also exposed as a Metal
/// texture. liberika renders into the IOSurface-backed Metal texture directly;
/// Flutter then composites the same buffer without a CPU readback.
private final class ErikaFlutterTextureSurface: NSObject, FlutterTexture {
  struct PreparedFrame {
    let generation: UInt64
    let pixelBuffer: CVPixelBuffer
    let cvMetalTexture: CVMetalTexture
    let metalTexture: MTLTexture
    let width: UInt32
    let height: UInt32
  }

  private let registry: FlutterTextureRegistry
  private let lock = NSLock()
  private let device: MTLDevice
  private var textureCache: CVMetalTextureCache
  private var pixelBufferPool: CVPixelBufferPool
  private var readyPixelBuffer: CVPixelBuffer?
  private var generation: UInt64 = 1
  private(set) var textureId: Int64 = 0
  private(set) var width: UInt32
  private(set) var height: UInt32
  private(set) var scale: Double
  weak var attachedPlayer: ErikaPlayerHost?

  init?(
    registry: FlutterTextureRegistry,
    width: UInt32,
    height: UInt32,
    scale: Double
  ) {
    guard let device = MTLCreateSystemDefaultDevice(),
          let resources = Self.makeResources(device: device, width: width, height: height) else {
      return nil
    }
    self.registry = registry
    self.device = device
    textureCache = resources.textureCache
    pixelBufferPool = resources.pixelBufferPool
    self.width = width
    self.height = height
    self.scale = scale
    super.init()
  }

  func register() -> Int64 {
    let id = registry.register(self)
    textureId = id
    return id
  }

  func resize(width: UInt32, height: UInt32, scale: Double) -> Bool {
    guard let resources = Self.makeResources(device: device, width: width, height: height) else {
      return false
    }
    lock.lock()
    generation &+= 1
    textureCache = resources.textureCache
    pixelBufferPool = resources.pixelBufferPool
    readyPixelBuffer = nil
    self.width = width
    self.height = height
    self.scale = scale
    lock.unlock()
    return true
  }

  func metrics() -> (width: UInt32, height: UInt32, scale: Double) {
    lock.lock()
    defer { lock.unlock() }
    return (width, height, scale)
  }

  func prepareFrame() -> PreparedFrame? {
    lock.lock()
    let cache = textureCache
    let pool = pixelBufferPool
    let frameWidth = width
    let frameHeight = height
    let frameGeneration = generation
    lock.unlock()

    var pixelBuffer: CVPixelBuffer?
    guard CVPixelBufferPoolCreatePixelBuffer(
      kCFAllocatorDefault,
      pool,
      &pixelBuffer
    ) == kCVReturnSuccess, let pixelBuffer else {
      return nil
    }
    var cvMetalTexture: CVMetalTexture?
    guard CVMetalTextureCacheCreateTextureFromImage(
      kCFAllocatorDefault,
      cache,
      pixelBuffer,
      nil,
      .bgra8Unorm,
      Int(frameWidth),
      Int(frameHeight),
      0,
      &cvMetalTexture
    ) == kCVReturnSuccess,
      let cvMetalTexture,
      let metalTexture = CVMetalTextureGetTexture(cvMetalTexture) else {
      return nil
    }
    return PreparedFrame(
      generation: frameGeneration,
      pixelBuffer: pixelBuffer,
      cvMetalTexture: cvMetalTexture,
      metalTexture: metalTexture,
      width: frameWidth,
      height: frameHeight
    )
  }

  func publish(_ frame: PreparedFrame) {
    lock.lock()
    guard frame.generation == generation else {
      lock.unlock()
      return
    }
    readyPixelBuffer = frame.pixelBuffer
    let id = textureId
    lock.unlock()
    if id != 0 {
      registry.textureFrameAvailable(id)
    }
  }

  func copyPixelBuffer() -> Unmanaged<CVPixelBuffer>? {
    lock.lock()
    defer { lock.unlock() }
    guard let readyPixelBuffer else {
      return nil
    }
    return Unmanaged.passRetained(readyPixelBuffer)
  }

  private static func makeResources(
    device: MTLDevice,
    width: UInt32,
    height: UInt32
  ) -> (textureCache: CVMetalTextureCache, pixelBufferPool: CVPixelBufferPool)? {
    var textureCache: CVMetalTextureCache?
    guard CVMetalTextureCacheCreate(
      kCFAllocatorDefault,
      nil,
      device,
      nil,
      &textureCache
    ) == kCVReturnSuccess, let textureCache else {
      return nil
    }
    let poolAttributes: [CFString: Any] = [
      kCVPixelBufferPoolMinimumBufferCountKey: 3,
    ]
    let pixelBufferAttributes: [CFString: Any] = [
      kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
      kCVPixelBufferWidthKey: Int(width),
      kCVPixelBufferHeightKey: Int(height),
      kCVPixelBufferIOSurfacePropertiesKey: [:],
      kCVPixelBufferMetalCompatibilityKey: true,
    ]
    var pixelBufferPool: CVPixelBufferPool?
    guard CVPixelBufferPoolCreate(
      kCFAllocatorDefault,
      poolAttributes as CFDictionary,
      pixelBufferAttributes as CFDictionary,
      &pixelBufferPool
    ) == kCVReturnSuccess, let pixelBufferPool else {
      return nil
    }
    return (textureCache, pixelBufferPool)
  }
}

private struct ErikaVideoParamsC {
  var width: UInt32 = 0
  var height: UInt32 = 0
  var primaries: UInt32 = 0
  var transfer: UInt32 = 0
}

private struct ErikaTrackCountsC {
  var video: UInt32 = 0
  var audio: UInt32 = 0
  var subtitle: UInt32 = 0
}

private struct ErikaTrackSelectionC {
  var video: Int64 = -1
  var audio: Int64 = -1
  var subtitle: Int64 = -1
}

private struct ErikaTrackInfoC {
  var id: Int64 = -1
  var kind: Int32 = 0
  var source: Int32 = 0
  var selected: UInt8 = 0
  var canRemove: UInt8 = 0
  var title: UnsafeMutablePointer<CChar>?
  var language: UnsafeMutablePointer<CChar>?
  var codec: UnsafeMutablePointer<CChar>?
  var width: UInt32 = 0
  var height: UInt32 = 0
  var sampleRate: UInt32 = 0
  var channels: UInt32 = 0
  var pixelFormat: UnsafeMutablePointer<CChar>?
  var sampleFormat: UnsafeMutablePointer<CChar>?
  var profile: UnsafeMutablePointer<CChar>?
  var level: Int32 = 0
  var bitRate: UInt64 = 0
  var frameRateNumerator: UInt32 = 0
  var frameRateDenominator: UInt32 = 0
}

private struct ErikaPresenterConfigC {
  var outputMode: Int32 = 0
  var edrHeadroom: Float = 1.0
  var lumaUpscaler: Int32 = 0
  var videoAlphaMode: Int32 = 0

  static let sdr = ErikaPresenterConfigC()

  static func appleEdr(headroom: Float) -> ErikaPresenterConfigC {
    ErikaPresenterConfigC(outputMode: 1, edrHeadroom: max(1.0, headroom))
  }

  static func auto(headroom: Float) -> ErikaPresenterConfigC {
    ErikaPresenterConfigC(outputMode: 3, edrHeadroom: max(1.0, headroom))
  }
}

private struct ErikaSubtitleStyleC {
  var fontFamily: UnsafePointer<CChar>?
  var fontFilePath: UnsafePointer<CChar>?
  var primaryColorRgba: UInt32
  var outlineColorRgba: UInt32
  var fontSize: Double
  var outlineWidth: Double
  var bold: Bool
  var italic: Bool
  var underline: Bool
  var strikeOut: Bool
  var spacing: Double
  var scaleXPercent: Double
  var scaleYPercent: Double
  var borderStyle: Int32
  var shadowDepth: Double
  var blur: Double
  var alignment: Int32
  var marginLeft: Int32
  var marginRight: Int32
  var marginVertical: Int32
  var overrideMask: UInt32
}

private struct ErikaHttpHeader {
  var name: UnsafeMutablePointer<CChar>?
  var value: UnsafeMutablePointer<CChar>?
}

/// Mirrors the C `ErikaOpenOptions`: headers plus per-request tuning.
/// `httpReadAheadBytes` of 0 uses the environment override, then 2 MiB;
/// `httpBackBufferBytes` of 0 uses the 16 MiB rewind-budget default.
private struct ErikaOpenOptions {
  var headers: UnsafeRawPointer?
  var headerCount: UInt = 0
  var httpReadAheadBytes: UInt64 = 0
  var httpBackBufferBytes: UInt64 = 0
  var reserved: (UInt64, UInt64) = (0, 0)
}

private struct ErikaGifExportOptionsC {
  var inputUri: UnsafePointer<CChar>?
  var outputPath: UnsafePointer<CChar>?
  var startMillis: UInt64
  var endMillis: UInt64
  var framesPerSecond: UInt32
  var outputWidth: UInt32
  var outputHeight: UInt32
  var quality: Int32
  var loopCount: Int32
  var overwrite: Bool
  var headers: UnsafeRawPointer?
  var headerCount: UInt
  var httpReadAheadBytes: UInt64
  var reserved: (UInt64, UInt64, UInt64) = (0, 0, 0)
}

private struct ErikaGifExportResultC {
  var width: UInt32 = 0
  var height: UInt32 = 0
  var frameCount: UInt64 = 0
  var fileSize: UInt64 = 0
}

private struct ErikaFrameExportOptionsC {
  var inputUri: UnsafePointer<CChar>?
  var headers: UnsafeRawPointer?
  var headerCount: UInt
  var positionMillis: UInt64
  var httpReadAheadBytes: UInt64
  var maxWidth: UInt32
  var maxHeight: UInt32
  var format: Int32
  var reservedFlags: UInt32 = 0
  var reserved: (UInt64, UInt64, UInt64, UInt64) = (0, 0, 0, 0)
}

private struct ErikaFrameExportResultC {
  var data: UnsafeMutablePointer<UInt8>?
  var dataLen: UInt = 0
  var timestampMicros: UInt64 = 0
  var durationMicros: UInt64 = 0
  var width: UInt32 = 0
  var height: UInt32 = 0
  var sourceWidth: UInt32 = 0
  var sourceHeight: UInt32 = 0
  var format: Int32 = 0
  var reservedFlags: UInt32 = 0
  var reserved: (UInt64, UInt64) = (0, 0)
}

private struct ErikaFrameExportSessionOptionsC {
  var inputUri: UnsafePointer<CChar>?
  var headers: UnsafeRawPointer?
  var headerCount: UInt
  var httpReadAheadBytes: UInt64
  var reservedFlags: UInt32 = 0
  var reserved: (UInt64, UInt64, UInt64, UInt64) = (0, 0, 0, 0)
}

private struct ErikaFrameExportRequestC {
  var positionMillis: UInt64
  var maxWidth: UInt32
  var maxHeight: UInt32
  var format: Int32
  var reservedFlags: UInt32 = 0
  var reserved: (UInt64, UInt64, UInt64, UInt64) = (0, 0, 0, 0)
}

private struct ErikaEventC {
  var kind: Int32 = 0
  var status: Int32 = 0
  var state: Int32 = 0
  var durationMicros: Int64 = -1
  var positionMicros: UInt64 = 0
  var buffering: UInt8 = 0
  var video: ErikaVideoParamsC = ErikaVideoParamsC()
  var tracks: ErikaTrackCountsC = ErikaTrackCountsC()
}

private struct ErikaPresenterStatsC {
  var decodedVideoFrames: UInt64 = 0
  var renderedVideoFrames: UInt64 = 0
  var renderedTestFrames: UInt64 = 0
  var pushedAudioFrames: UInt64 = 0
  var overlayFrames: UInt64 = 0
  var danmakuFrames: UInt64 = 0
  var danmakuItems: UInt64 = 0
  var importFailures: UInt64 = 0
  var renderFailures: UInt64 = 0
  var audioFailures: UInt64 = 0
  // The fields below must mirror `ErikaPresenterStats` in
  // crates/erika_capi/include/erika.h exactly (order and types). erika_presenter_render_tick
  // writes the full struct through this pointer, so any missing field overflows the buffer.
  var softwareVideoFrames: UInt64 = 0
  var hardwareVideoFrames: UInt64 = 0
  var zeroCopyVideoFrames: UInt64 = 0
  var cpuVideoFrameFallbacks: UInt64 = 0
  var lastRenderMicros: UInt64 = 0
  var lastRenderCurrentMicros: UInt64 = 0
  var audioClockReadFrames: UInt64 = 0
  var audioClockQueuedFrames: UInt64 = 0
  var audioClockUnderflowFrames: UInt64 = 0
  var audioRecoveryState: Int32 = 0
  var audioLastErrorCode: Int32 = 0
  var audioRecoveryAttempts: UInt64 = 0
  var audioRecoveryCount: UInt64 = 0
  var audioRecoveryFailures: UInt64 = 0
  var directZeroCopyVideoFrames: UInt64 = 0
  var sharedHandleVideoFrames: UInt64 = 0
  var hdrSourceFrames: UInt64 = 0
  var hdr10OutputFrames: UInt64 = 0
  var sdrTonemapFrames: UInt64 = 0
  var hdr10MetadataUpdates: UInt64 = 0
  var hdr10MetadataFailures: UInt64 = 0
  var hdr10OutputFailures: UInt64 = 0
  var hdr10OutputActive: Bool = false
  var videoFrameBackpressureDrops: UInt64 = 0
}

private struct ErikaUpscalerStatusC {
  var requestedMode: Int32 = 0
  var activeBackend: Int32 = 0
  var fallbackCount: UInt64 = 0
  var upscaledFrames: UInt64 = 0
  var lastEncodeMicros: UInt64 = 0
  var lastGpuMicros: UInt64 = 0
}

private struct ErikaSubtitleMemoryFontStatusC {
  var registeredCount: UInt = 0
  var registeredBytes: UInt = 0
  var selectedCount: UInt = 0
  var generation: UInt64 = 0
  var selectedIds: UnsafeMutablePointer<UInt64>?
}

// Keep field order and types aligned with `ErikaOutputStatus` in erika.h.
private struct ErikaOutputStatusC {
  var requestedMode: Int32 = 0
  var activeEncoding: Int32 = 0
  var surfaceFormat: Int32 = 0
  var nativeDataSpace: Int32 = 0
  var requestedHeadroom: Float = 1.0
  var activeHeadroom: Float = 1.0
  var activeHeadroomKnown: Bool = false
  var extendedLinearActive: Bool = false
  var fallbackReason: Int32 = 0
  var fallbackCount: UInt64 = 0
  var dataSpaceFailures: UInt64 = 0
  var headroomUpdates: UInt64 = 0
  var extendedLinearFrames: UInt64 = 0
}

// Keep field order and types aligned with `ErikaPresenterResourceStatus` in erika.h.
private struct ErikaPresenterResourceStatusC {
  var deviceCurrentAllocatedBytes: UInt64 = 0
  var deviceRecommendedWorkingSetBytes: UInt64 = 0
  var drawableEstimatedBytes: UInt64 = 0
  var videoFrameBytes: UInt64 = 0
  var overlayAtlasBytes: UInt64 = 0
  var danmakuAtlasBytes: UInt64 = 0
  var danmakuVertexBufferBytes: UInt64 = 0
  var upscalerBytes: UInt64 = 0
  var rendererTrackedBytes: UInt64 = 0
  var presenterCpuDanmakuAtlasBytes: UInt64 = 0
  var drawableCount: UInt32 = 0
  var outputModeSwitches: UInt64 = 0
}

private struct ErikaDanmakuConfigC {
  var enabled: UInt8 = 1
  // NipaPlay/Flutter logical font size; liberika applies the surface scale internally.
  var fontSize: Float = 30.0
  var opacity: Float = 1.0
  var displayArea: Float = 1.0
  var scrollDurationSeconds: Float = 10.0
  var scrollSpeedFactor: Float = 1.0
  var trackGapRatio: Float = 0.15
  var outlineWidth: Float = 1.0
  var shadowOffsetX: Float = 1.0
  var shadowOffsetY: Float = 1.0
  var mergeDuplicates: UInt8 = 0
  var allowStacking: UInt8 = 0
  var allowScrollOverwrite: UInt8 = 1
  var maxQuantity: UInt32 = 0
  var maxLinesPerMode: UInt32 = 0
  var blockTop: UInt8 = 0
  var blockBottom: UInt8 = 0
  var blockScroll: UInt8 = 0
  var shadowStyle: Int32 = 3
}

private struct ErikaDanmakuTrackInfoC {
  var id: UInt64 = 0
  var enabled: UInt8 = 0
  var offsetMicros: Int64 = 0
  var itemCount: Int = 0
  var name: UnsafeMutablePointer<CChar>?
  var source: UnsafeMutablePointer<CChar>?
}

private enum ErikaPluginError: Error, CustomStringConvertible {
  case libraryNotFound([String])
  case symbolMissing(String)
  case httpHeadersUnsupported
  case openOptionsUnsupported
  case invalidArguments(String)
  case playerNotFound(Int64)
  case viewNotFound(Int64)
  case overlayNotAvailable
  case presenterCreateFailed
  case erikaStatus(String, Int32)
  case libraryLoadFailed(String, String?)
  case diagnosticsUnsupported(String)
  case diagnosticsFailed(String)

  var description: String {
    switch self {
    case .libraryNotFound(let paths):
      return "Unable to load liberika_capi.dylib. Tried: \(paths.joined(separator: ", "))"
    case .symbolMissing(let symbol):
      return "Missing Erika C ABI symbol: \(symbol)"
    case .httpHeadersUnsupported:
      return "The loaded Erika native library does not export erika_presenter_open_with_headers, so httpHeaders cannot be applied. Update the bundled native library (a prebuilt from 0.1.3 or earlier predates HTTP header support)."
    case .openOptionsUnsupported:
      return "The loaded Erika native library does not export erika_presenter_open_with_options, so httpReadAheadBytes / httpBackBufferBytes cannot be applied. Update the bundled native library (a prebuilt from 0.1.7 or earlier predates open options support)."
    case .invalidArguments(let message):
      return message
    case .playerNotFound(let playerId):
      return "Erika player \(playerId) was not found."
    case .viewNotFound(let viewId):
      return "Erika video view \(viewId) was not found."
    case .overlayNotAvailable:
      return "No window-hosted Erika overlay is available."
    case .presenterCreateFailed:
      return "erika_presenter_create returned null."
    case .erikaStatus(let operation, let status):
      return "\(operation) failed with ErikaStatus \(status)."
    case .libraryLoadFailed(let path, let detail):
      if let detail, !detail.isEmpty {
        return "\(path) (\(detail))"
      }
      return path
    case .diagnosticsUnsupported(let symbol):
      return "The loaded Erika native library does not export \(symbol). Update the bundled native library to use this method."
    case .diagnosticsFailed(let message):
      return message
    }
  }
}

private final class ErikaNativeLibrary {
  // This symbol was added after ErikaTrackInfo gained its extended media
  // metadata fields. Loading an older dylib with the current Swift struct
  // would use a different record stride and corrupt the following pointers.
  private static let currentTrackInfoAbiSymbol = "erika_presenter_get_output_status"

  typealias CreateFn = @convention(c) () -> UnsafeMutableRawPointer?
  typealias CreateWithOutputModeFn = @convention(c) (Int32, Float) -> UnsafeMutableRawPointer?
  typealias CreateWithOutputModeAndAlphaFn = @convention(c) (
    Int32, Float, Int32
  ) -> UnsafeMutableRawPointer?
  typealias DestroyFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias OpenFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Int32
  typealias OpenWithHeadersFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafeRawPointer?, UInt) -> Int32
  typealias OpenWithOptionsFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafeRawPointer?) -> Int32
  typealias CommandFn = @convention(c) (UnsafeMutableRawPointer?) -> Int32
  typealias SeekFn = @convention(c) (UnsafeMutableRawPointer?, UInt64) -> Int32
  typealias SetPlaybackRateFn = @convention(c) (UnsafeMutableRawPointer?, Double) -> Int32
  typealias SetVolumeFn = @convention(c) (UnsafeMutableRawPointer?, Double) -> Int32
  typealias SetUpscalerFn = @convention(c) (UnsafeMutableRawPointer?, Int32) -> Int32
  typealias SetSubtitleScaleFn = @convention(c) (UnsafeMutableRawPointer?, Double) -> Int32
  typealias SetSubtitleFontFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?
  ) -> Int32
  typealias SetSubtitleStyleFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafeRawPointer?
  ) -> Int32
  typealias RegisterSubtitleMemoryFontFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, UInt, UnsafeMutablePointer<UInt64>?) -> Int32
  typealias SelectSubtitleMemoryFontsFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt64>?, UInt) -> Int32
  typealias GetSubtitleMemoryFontStatusFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias FreeSubtitleMemoryFontStatusFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias GetUpscalerStatusFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias GetOutputStatusFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias GetResourceStatusFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias SelectTrackFn = @convention(c) (UnsafeMutableRawPointer?, Int64) -> Int32
  typealias AddExternalSubtitleFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<CChar>?,
    UnsafeMutablePointer<Int64>?
  ) -> Int32
  typealias RemoveSubtitleTrackFn = @convention(c) (UnsafeMutableRawPointer?, Int64) -> Int32
  typealias LoadDanmakuFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Int32
  typealias AddDanmakuTrackFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?,
    Int64,
    UnsafeMutablePointer<UInt64>?
  ) -> Int32
  typealias ClearDanmakuFn = @convention(c) (UnsafeMutableRawPointer?) -> Int32
  typealias SetDanmakuEnabledFn = @convention(c) (UnsafeMutableRawPointer?, Bool) -> Int32
  typealias SetDebugHudEnabledFn = @convention(c) (UnsafeMutableRawPointer?, Bool) -> Int32
  typealias SetDanmakuConfigFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeRawPointer?) -> Int32
  typealias GetDanmakuConfigFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias SetDanmakuFontFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?
  ) -> Int32
  typealias SetDanmakuBlockWordsFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Int32
  typealias RemoveDanmakuTrackFn = @convention(c) (UnsafeMutableRawPointer?, UInt64) -> Int32
  typealias SetDanmakuTrackEnabledFn = @convention(c) (UnsafeMutableRawPointer?, UInt64, Bool) -> Int32
  typealias SetDanmakuTrackOffsetFn = @convention(c) (UnsafeMutableRawPointer?, UInt64, Int64) -> Int32
  typealias SetDanmakuGlobalOffsetFn = @convention(c) (UnsafeMutableRawPointer?, Int64) -> Int32
  typealias TrackSelectionFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias TracksFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafeMutableRawPointer?,
    Int,
    UnsafeMutablePointer<Int>?
  ) -> Int32
  typealias TrackInfoFreeFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias DanmakuTrackInfoFreeFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias AttachMetalLayerFn = @convention(c) (UnsafeMutableRawPointer?, UInt64, UInt32, UInt32, Double) -> Int32
  typealias AttachFlutterTextureFn = @convention(c) (
    UnsafeMutableRawPointer?, Int32, Int64, UInt32, UInt32, Double
  ) -> Int32
  typealias SetFlutterTextureBufferFn = @convention(c) (
    UnsafeMutableRawPointer?, UInt64, UInt32, UInt32
  ) -> Int32
  typealias ResizeSurfaceFn = @convention(c) (UnsafeMutableRawPointer?, UInt32, UInt32, Double) -> Int32
  typealias RenderTickFn = @convention(c) (UnsafeMutableRawPointer?, Double, UnsafeMutableRawPointer?) -> Int32
  typealias SetOutputHeadroomFn = @convention(c) (UnsafeMutableRawPointer?, Float, Bool) -> Int32
  typealias CaptureFrameRgbaFn = @convention(c) (UnsafeMutableRawPointer?, UInt32, UInt32, UnsafeMutableRawPointer?, Int) -> Int32
  typealias ExportGifFn = @convention(c) (UnsafeRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias ExportFrameFn = @convention(c) (UnsafeRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias FrameExportResultFreeFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias FrameExportSessionOpenFn = @convention(c) (
    UnsafeRawPointer?,
    UnsafeMutablePointer<UnsafeMutableRawPointer?>?
  ) -> Int32
  typealias FrameExportSessionFrameFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafeRawPointer?,
    UnsafeMutableRawPointer?
  ) -> Int32
  typealias FrameExportSessionCloseFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias PollEventFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias LastErrorMessageFn = @convention(c) () -> UnsafeMutablePointer<CChar>?
  typealias StringFreeFn = @convention(c) (UnsafeMutablePointer<CChar>?) -> Void
  // Diagnostics return caller-owned JSON strings (no mirrored structs), so
  // both symbols are optional: older native libraries simply lack them.
  typealias GetDiagnosticsJsonFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
  ) -> Int32
  typealias InvokeJsonFn = @convention(c) (
    UnsafeMutableRawPointer?,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?
  ) -> UnsafeMutablePointer<CChar>?
  typealias LogReadJsonFn = @convention(c) (
    UInt32,
    Int32,
    UInt64,
    UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
  ) -> Int32

  static let shared = try? ErikaNativeLibrary()

  let create: CreateFn
  let createWithOutputMode: CreateWithOutputModeFn?
  let createWithOutputModeAndAlpha: CreateWithOutputModeAndAlphaFn?
  let destroy: DestroyFn
  let open: OpenFn
  let openWithHeaders: OpenWithHeadersFn?
  let openWithOptions: OpenWithOptionsFn?
  let play: CommandFn
  let pause: CommandFn
  let stop: CommandFn
  let close: CommandFn
  let seek: SeekFn
  let setPlaybackRate: SetPlaybackRateFn?
  let setVolume: SetVolumeFn?
  let setUpscaler: SetUpscalerFn?
  let setSubtitleScale: SetSubtitleScaleFn?
  let setSubtitleFont: SetSubtitleFontFn?
  let setSubtitleStyle: SetSubtitleStyleFn?
  let registerSubtitleMemoryFont: RegisterSubtitleMemoryFontFn?
  let selectSubtitleMemoryFonts: SelectSubtitleMemoryFontsFn?
  let clearSubtitleMemoryFonts: CommandFn?
  let getSubtitleMemoryFontStatus: GetSubtitleMemoryFontStatusFn?
  let freeSubtitleMemoryFontStatus: FreeSubtitleMemoryFontStatusFn?
  let getUpscalerStatus: GetUpscalerStatusFn?
  let getOutputStatus: GetOutputStatusFn?
  let getResourceStatus: GetResourceStatusFn?
  let selectAudioTrack: SelectTrackFn
  let selectSubtitleTrack: SelectTrackFn
  let addExternalSubtitle: AddExternalSubtitleFn
  let removeSubtitleTrack: RemoveSubtitleTrackFn
  let loadDanmakuFile: LoadDanmakuFn?
  let loadDanmakuJson: LoadDanmakuFn?
  let addDanmakuTrackFile: AddDanmakuTrackFn?
  let addDanmakuTrackJson: AddDanmakuTrackFn?
  let removeDanmakuTrack: RemoveDanmakuTrackFn?
  let setDanmakuTrackEnabled: SetDanmakuTrackEnabledFn?
  let setDanmakuTrackOffset: SetDanmakuTrackOffsetFn?
  let setDanmakuGlobalOffset: SetDanmakuGlobalOffsetFn?
  let danmakuTracks: TracksFn?
  let clearDanmaku: ClearDanmakuFn?
  let setDanmakuEnabled: SetDanmakuEnabledFn?
  let setDebugHudEnabled: SetDebugHudEnabledFn?
  let setDanmakuConfig: SetDanmakuConfigFn?
  let getDanmakuConfig: GetDanmakuConfigFn?
  let setDanmakuFont: SetDanmakuFontFn?
  let setDanmakuBlockWords: SetDanmakuBlockWordsFn?
  let trackSelection: TrackSelectionFn
  let tracks: TracksFn
  let freeTrackInfo: TrackInfoFreeFn
  let freeDanmakuTrackInfo: DanmakuTrackInfoFreeFn?
  let attachMetalLayer: AttachMetalLayerFn
  let attachFlutterTexture: AttachFlutterTextureFn
  let setFlutterTextureBuffer: SetFlutterTextureBufferFn
  let resizeSurface: ResizeSurfaceFn
  let detachSurface: CommandFn
  let renderTick: RenderTickFn
  let captureFrameRgba: CaptureFrameRgbaFn?
  let exportGif: ExportGifFn?
  let exportFrame: ExportFrameFn?
  let frameExportResultFree: FrameExportResultFreeFn?
  let frameExportSessionOpen: FrameExportSessionOpenFn?
  let frameExportSessionFrame: FrameExportSessionFrameFn?
  let frameExportSessionClose: FrameExportSessionCloseFn?
  private let globalJsonMethodsLock = NSLock()
  private var globalJsonMethods: Set<String>?
  let pollEvent: PollEventFn
  let setOutputHeadroom: SetOutputHeadroomFn?
  let lastErrorMessage: LastErrorMessageFn
  let stringFree: StringFreeFn
  let getDiagnosticsJson: GetDiagnosticsJsonFn?
  let logReadJson: LogReadJsonFn?
  let invokeJson: InvokeJsonFn?

  private let libraryHandle: UnsafeMutableRawPointer

  private init() throws {
    let loaded = try Self.openLibrary()
    libraryHandle = loaded.handle

    create = try Self.load("erika_presenter_create", from: libraryHandle, as: CreateFn.self)
    createWithOutputMode = Self.loadOptional("erika_presenter_create_with_output_mode", from: libraryHandle, as: CreateWithOutputModeFn.self)
    createWithOutputModeAndAlpha = Self.loadOptional(
      "erika_presenter_create_with_output_mode_and_alpha",
      from: libraryHandle,
      as: CreateWithOutputModeAndAlphaFn.self
    )
    destroy = try Self.load("erika_presenter_destroy", from: libraryHandle, as: DestroyFn.self)
    open = try Self.load("erika_presenter_open", from: libraryHandle, as: OpenFn.self)
    openWithHeaders = Self.loadOptional("erika_presenter_open_with_headers", from: libraryHandle, as: OpenWithHeadersFn.self)
    openWithOptions = Self.loadOptional("erika_presenter_open_with_options", from: libraryHandle, as: OpenWithOptionsFn.self)
    play = try Self.load("erika_presenter_play", from: libraryHandle, as: CommandFn.self)
    pause = try Self.load("erika_presenter_pause", from: libraryHandle, as: CommandFn.self)
    stop = try Self.load("erika_presenter_stop", from: libraryHandle, as: CommandFn.self)
    close = try Self.load("erika_presenter_close", from: libraryHandle, as: CommandFn.self)
    seek = try Self.load("erika_presenter_seek", from: libraryHandle, as: SeekFn.self)
    setPlaybackRate = Self.loadOptional("erika_presenter_set_playback_rate", from: libraryHandle, as: SetPlaybackRateFn.self)
    setVolume = Self.loadOptional("erika_presenter_set_volume", from: libraryHandle, as: SetVolumeFn.self)
    setUpscaler = Self.loadOptional("erika_presenter_set_upscaler", from: libraryHandle, as: SetUpscalerFn.self)
    setSubtitleScale = Self.loadOptional("erika_presenter_set_subtitle_scale", from: libraryHandle, as: SetSubtitleScaleFn.self)
    setSubtitleFont = Self.loadOptional("erika_presenter_set_subtitle_font", from: libraryHandle, as: SetSubtitleFontFn.self)
    setSubtitleStyle = Self.loadOptional("erika_presenter_set_subtitle_style", from: libraryHandle, as: SetSubtitleStyleFn.self)
    registerSubtitleMemoryFont = Self.loadOptional("erika_presenter_register_subtitle_memory_font", from: libraryHandle, as: RegisterSubtitleMemoryFontFn.self)
    selectSubtitleMemoryFonts = Self.loadOptional("erika_presenter_select_subtitle_memory_fonts", from: libraryHandle, as: SelectSubtitleMemoryFontsFn.self)
    clearSubtitleMemoryFonts = Self.loadOptional("erika_presenter_clear_subtitle_memory_fonts", from: libraryHandle, as: CommandFn.self)
    getSubtitleMemoryFontStatus = Self.loadOptional("erika_presenter_get_subtitle_memory_font_status", from: libraryHandle, as: GetSubtitleMemoryFontStatusFn.self)
    freeSubtitleMemoryFontStatus = Self.loadOptional("erika_subtitle_memory_font_status_free", from: libraryHandle, as: FreeSubtitleMemoryFontStatusFn.self)
    getUpscalerStatus = Self.loadOptional("erika_presenter_get_upscaler_status", from: libraryHandle, as: GetUpscalerStatusFn.self)
    getOutputStatus = Self.loadOptional("erika_presenter_get_output_status", from: libraryHandle, as: GetOutputStatusFn.self)
    getResourceStatus = Self.loadOptional("erika_presenter_get_resource_status", from: libraryHandle, as: GetResourceStatusFn.self)
    selectAudioTrack = try Self.load("erika_presenter_select_audio_track", from: libraryHandle, as: SelectTrackFn.self)
    selectSubtitleTrack = try Self.load("erika_presenter_select_subtitle_track", from: libraryHandle, as: SelectTrackFn.self)
    addExternalSubtitle = try Self.load("erika_presenter_add_external_subtitle", from: libraryHandle, as: AddExternalSubtitleFn.self)
    removeSubtitleTrack = try Self.load("erika_presenter_remove_subtitle_track", from: libraryHandle, as: RemoveSubtitleTrackFn.self)
    loadDanmakuFile = Self.loadOptional("erika_presenter_load_danmaku_file", from: libraryHandle, as: LoadDanmakuFn.self)
    loadDanmakuJson = Self.loadOptional("erika_presenter_load_danmaku_json", from: libraryHandle, as: LoadDanmakuFn.self)
    addDanmakuTrackFile = Self.loadOptional("erika_presenter_add_danmaku_track_file", from: libraryHandle, as: AddDanmakuTrackFn.self)
    addDanmakuTrackJson = Self.loadOptional("erika_presenter_add_danmaku_track_json", from: libraryHandle, as: AddDanmakuTrackFn.self)
    removeDanmakuTrack = Self.loadOptional("erika_presenter_remove_danmaku_track", from: libraryHandle, as: RemoveDanmakuTrackFn.self)
    setDanmakuTrackEnabled = Self.loadOptional("erika_presenter_set_danmaku_track_enabled", from: libraryHandle, as: SetDanmakuTrackEnabledFn.self)
    setDanmakuTrackOffset = Self.loadOptional("erika_presenter_set_danmaku_track_offset", from: libraryHandle, as: SetDanmakuTrackOffsetFn.self)
    setDanmakuGlobalOffset = Self.loadOptional("erika_presenter_set_danmaku_global_offset", from: libraryHandle, as: SetDanmakuGlobalOffsetFn.self)
    danmakuTracks = Self.loadOptional("erika_presenter_danmaku_tracks", from: libraryHandle, as: TracksFn.self)
    clearDanmaku = Self.loadOptional("erika_presenter_clear_danmaku", from: libraryHandle, as: ClearDanmakuFn.self)
    setDanmakuEnabled = Self.loadOptional("erika_presenter_set_danmaku_enabled", from: libraryHandle, as: SetDanmakuEnabledFn.self)
    setDebugHudEnabled = Self.loadOptional("erika_presenter_set_debug_hud_enabled", from: libraryHandle, as: SetDebugHudEnabledFn.self)
    setDanmakuConfig = Self.loadOptional("erika_presenter_set_danmaku_config_ptr", from: libraryHandle, as: SetDanmakuConfigFn.self)
    getDanmakuConfig = Self.loadOptional("erika_presenter_get_danmaku_config", from: libraryHandle, as: GetDanmakuConfigFn.self)
    setDanmakuFont = Self.loadOptional("erika_presenter_set_danmaku_font", from: libraryHandle, as: SetDanmakuFontFn.self)
    setDanmakuBlockWords = Self.loadOptional("erika_presenter_set_danmaku_block_words_json", from: libraryHandle, as: SetDanmakuBlockWordsFn.self)
    trackSelection = try Self.load("erika_presenter_track_selection", from: libraryHandle, as: TrackSelectionFn.self)
    tracks = try Self.load("erika_presenter_tracks", from: libraryHandle, as: TracksFn.self)
    freeTrackInfo = try Self.load("erika_track_info_free", from: libraryHandle, as: TrackInfoFreeFn.self)
    freeDanmakuTrackInfo = Self.loadOptional("erika_danmaku_track_info_free", from: libraryHandle, as: DanmakuTrackInfoFreeFn.self)
    attachMetalLayer = try Self.load("erika_presenter_attach_metal_layer", from: libraryHandle, as: AttachMetalLayerFn.self)
    attachFlutterTexture = try Self.load("erika_presenter_attach_flutter_texture", from: libraryHandle, as: AttachFlutterTextureFn.self)
    setFlutterTextureBuffer = try Self.load("erika_presenter_set_flutter_texture_buffer", from: libraryHandle, as: SetFlutterTextureBufferFn.self)
    resizeSurface = try Self.load("erika_presenter_resize_surface", from: libraryHandle, as: ResizeSurfaceFn.self)
    detachSurface = try Self.load("erika_presenter_detach_surface", from: libraryHandle, as: CommandFn.self)
    renderTick = try Self.load("erika_presenter_render_tick", from: libraryHandle, as: RenderTickFn.self)
    captureFrameRgba = Self.loadOptional("erika_presenter_capture_frame_rgba", from: libraryHandle, as: CaptureFrameRgbaFn.self)
    exportGif = Self.loadOptional("erika_export_gif", from: libraryHandle, as: ExportGifFn.self)
    exportFrame = Self.loadOptional("erika_export_frame", from: libraryHandle, as: ExportFrameFn.self)
    frameExportResultFree = Self.loadOptional(
      "erika_frame_export_result_free",
      from: libraryHandle,
      as: FrameExportResultFreeFn.self
    )
    frameExportSessionOpen = Self.loadOptional(
      "erika_frame_export_session_open",
      from: libraryHandle,
      as: FrameExportSessionOpenFn.self
    )
    frameExportSessionFrame = Self.loadOptional(
      "erika_frame_export_session_frame",
      from: libraryHandle,
      as: FrameExportSessionFrameFn.self
    )
    frameExportSessionClose = Self.loadOptional(
      "erika_frame_export_session_close",
      from: libraryHandle,
      as: FrameExportSessionCloseFn.self
    )
    pollEvent = try Self.load("erika_presenter_poll_event", from: libraryHandle, as: PollEventFn.self)
    setOutputHeadroom = Self.loadOptional("erika_presenter_set_output_headroom", from: libraryHandle, as: SetOutputHeadroomFn.self)
    lastErrorMessage = try Self.load("erika_last_error_message", from: libraryHandle, as: LastErrorMessageFn.self)
    stringFree = try Self.load("erika_string_free", from: libraryHandle, as: StringFreeFn.self)
    getDiagnosticsJson = Self.loadOptional("erika_presenter_get_diagnostics_json", from: libraryHandle, as: GetDiagnosticsJsonFn.self)
    logReadJson = Self.loadOptional("erika_log_read_json", from: libraryHandle, as: LogReadJsonFn.self)
    invokeJson = Self.loadOptional("erika_presenter_invoke_json", from: libraryHandle, as: InvokeJsonFn.self)
  }

  deinit {
    dlclose(libraryHandle)
  }

  private static func openLibrary() throws -> (handle: UnsafeMutableRawPointer, path: String) {
    let environment = ProcessInfo.processInfo.environment
    let bundle = Bundle(for: ErikaFlutterPlugin.self)
    var candidates: [String] = []

    if let explicitPath = environment["ERIKA_CAPI_DYLIB"], !explicitPath.isEmpty {
      candidates.append(explicitPath)
    }
    // CocoaPods builds the dylib inside the plugin framework. Prefer that
    // freshly built copy over an app-level dylib that may be left behind by an
    // older build.
    if let appFrameworksPath = Bundle.main.privateFrameworksPath {
      candidates.append(
        URL(fileURLWithPath: appFrameworksPath)
          .appendingPathComponent(
            "erika_flutter.framework/Versions/A/Frameworks/liberika_capi.dylib"
          )
          .path
      )
    }
    if let pluginExecutablePath = bundle.executablePath {
      candidates.append(
        URL(fileURLWithPath: pluginExecutablePath)
          .deletingLastPathComponent()
          .appendingPathComponent("Frameworks/liberika_capi.dylib")
          .path
      )
    }
    if let pluginFrameworksPath = bundle.privateFrameworksPath {
      candidates.append(
        URL(fileURLWithPath: pluginFrameworksPath)
          .appendingPathComponent("liberika_capi.dylib")
          .path
      )
    }
    if let resourcePath = bundle.path(forResource: "liberika_capi", ofType: "dylib") {
      candidates.append(resourcePath)
    }
    if let frameworkPath = Bundle.main.privateFrameworksPath {
      candidates.append(URL(fileURLWithPath: frameworkPath).appendingPathComponent("liberika_capi.dylib").path)
    }
    if let executablePath = Bundle.main.executablePath {
      let executableDirectory = URL(fileURLWithPath: executablePath).deletingLastPathComponent().path
      candidates.append(URL(fileURLWithPath: executableDirectory).appendingPathComponent("liberika_capi.dylib").path)
    }
    if let sourceTreePath = Self.sourceTreeDebugLibraryPath() {
      candidates.append(sourceTreePath)
    }
    candidates.append("liberika_capi.dylib")

    var failures: [ErikaPluginError] = []
    for path in candidates {
      if let handle = dlopen(path, RTLD_NOW | RTLD_LOCAL) {
        guard dlsym(handle, currentTrackInfoAbiSymbol) != nil else {
          dlclose(handle)
          failures.append(
            .libraryLoadFailed(
              path,
              "incompatible Erika C ABI: missing \(currentTrackInfoAbiSymbol)"
            )
          )
          continue
        }
        NSLog("ErikaFlutterPlugin: loaded Erika C API from \(path)")
        return (handle, path)
      }
      let detail = dlerror().map { String(cString: $0) }
      failures.append(.libraryLoadFailed(path, detail))
    }
    throw ErikaPluginError.libraryNotFound(failures.map(String.init(describing:)))
  }

  fileprivate static func sourceTreeDebugLibraryPath() -> String? {
    let sourceFile = URL(fileURLWithPath: #filePath)
    let erikaRoot = sourceFile
      .deletingLastPathComponent() // Classes
      .deletingLastPathComponent() // macos
      .deletingLastPathComponent() // erika_flutter
      .deletingLastPathComponent() // packages
      .deletingLastPathComponent() // Erika repo root
    return erikaRoot
      .appendingPathComponent("target")
      .appendingPathComponent("debug")
      .appendingPathComponent("liberika_capi.dylib")
      .path
  }

  private static func load<T>(_ symbol: String, from handle: UnsafeMutableRawPointer, as type: T.Type) throws -> T {
    guard let raw = dlsym(handle, symbol) else {
      throw ErikaPluginError.symbolMissing(symbol)
    }
    return unsafeBitCast(raw, to: type)
  }

  private static func loadOptional<T>(_ symbol: String, from handle: UnsafeMutableRawPointer, as type: T.Type) -> T? {
    guard let raw = dlsym(handle, symbol) else {
      return nil
    }
    return unsafeBitCast(raw, to: type)
  }

  func createPresenter(config: ErikaPresenterConfigC) -> UnsafeMutableRawPointer? {
    if let createWithOutputModeAndAlpha {
      return createWithOutputModeAndAlpha(
        config.outputMode,
        config.edrHeadroom,
        config.videoAlphaMode
      )
    }
    if config.videoAlphaMode != 0 {
      return nil
    }
    if let createWithOutputMode {
      return createWithOutputMode(config.outputMode, config.edrHeadroom)
    }
    return create()
  }

  /// One picture through liberika's headless frame export, copied out of the
  /// native buffer, which is released before returning. Runs on the calling
  /// thread: call it from a worker queue.
  func exportFrame(
    inputUri: String,
    positionMillis: UInt64,
    maxWidth: UInt32,
    maxHeight: UInt32,
    format: Int32,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64
  ) throws -> [String: Any] {
    guard let exportFrame, let frameExportResultFree else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_export_frame")
    }
    let names = httpHeaders.keys.map { strdup($0) }
    let values = httpHeaders.values.map { strdup($0) }
    defer {
      names.forEach { free($0) }
      values.forEach { free($0) }
    }
    let headers = zip(names, values).map { ErikaHttpHeader(name: $0.0, value: $0.1) }
    return try inputUri.withCString { inputPtr in
      try headers.withUnsafeBufferPointer { buffer in
        var options = ErikaFrameExportOptionsC(
          inputUri: inputPtr,
          headers: buffer.baseAddress.map(UnsafeRawPointer.init),
          headerCount: UInt(headers.count),
          positionMillis: positionMillis,
          httpReadAheadBytes: httpReadAheadBytes,
          maxWidth: maxWidth,
          maxHeight: maxHeight,
          format: format
        )
        var exported = ErikaFrameExportResultC()
        let status = withUnsafePointer(to: &options) { optionsPtr in
          withUnsafeMutablePointer(to: &exported) { resultPtr in
            exportFrame(UnsafeRawPointer(optionsPtr), UnsafeMutableRawPointer(resultPtr))
          }
        }
        if status != 0 {
          let detail = currentEventMessage() ?? "ErikaStatus \(status)"
          throw ErikaPluginError.diagnosticsFailed("Frame export failed: \(detail)")
        }
        defer {
          withUnsafeMutablePointer(to: &exported) { resultPtr in
            frameExportResultFree(UnsafeMutableRawPointer(resultPtr))
          }
        }
        return frameExportMap(exported)
      }
    }
  }

  /// A result of the frame export calls as the map Dart reads; the bytes are
  /// copied out of the native buffer, which the caller releases afterwards.
  private func frameExportMap(_ exported: ErikaFrameExportResultC) -> [String: Any] {
    let bytes = exported.data.map { Data(bytes: $0, count: Int(exported.dataLen)) } ?? Data()
    return [
      "width": Int(exported.width),
      "height": Int(exported.height),
      "format": Int(exported.format),
      "bytes": FlutterStandardTypedData(bytes: bytes),
      "timestampMicros": Int64(clamping: exported.timestampMicros),
      "durationMicros": Int64(clamping: exported.durationMicros),
      "sourceWidth": Int(exported.sourceWidth),
      "sourceHeight": Int(exported.sourceHeight),
      "toneMapped": exported.reservedFlags & 1 != 0,
    ]
  }

  /// Opens a frame export session. Runs on the calling thread (the container
  /// header is read): call it from the session's queue.
  func openFrameExportSession(
    inputUri: String,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64
  ) throws -> UnsafeMutableRawPointer {
    guard let open = frameExportSessionOpen,
          frameExportSessionFrame != nil,
          frameExportSessionClose != nil,
          frameExportResultFree != nil else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_frame_export_session_open")
    }
    let names = httpHeaders.keys.map { strdup($0) }
    let values = httpHeaders.values.map { strdup($0) }
    defer {
      names.forEach { free($0) }
      values.forEach { free($0) }
    }
    let headers = zip(names, values).map { ErikaHttpHeader(name: $0.0, value: $0.1) }
    return try inputUri.withCString { inputPtr in
      try headers.withUnsafeBufferPointer { buffer in
        var options = ErikaFrameExportSessionOptionsC(
          inputUri: inputPtr,
          headers: buffer.baseAddress.map(UnsafeRawPointer.init),
          headerCount: UInt(headers.count),
          httpReadAheadBytes: httpReadAheadBytes
        )
        var session: UnsafeMutableRawPointer?
        let status = withUnsafePointer(to: &options) { optionsPtr in
          open(UnsafeRawPointer(optionsPtr), &session)
        }
        guard status == 0, let session else {
          let detail = currentEventMessage() ?? "ErikaStatus \(status)"
          throw ErikaPluginError.diagnosticsFailed("Frame export failed: \(detail)")
        }
        return session
      }
    }
  }

  /// One picture from an open session, on the calling thread (the session's
  /// queue).
  func exportSessionFrame(
    session: UnsafeMutableRawPointer,
    positionMillis: UInt64,
    maxWidth: UInt32,
    maxHeight: UInt32,
    format: Int32
  ) throws -> [String: Any] {
    guard let frame = frameExportSessionFrame, let frameExportResultFree else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_frame_export_session_frame")
    }
    var request = ErikaFrameExportRequestC(
      positionMillis: positionMillis,
      maxWidth: maxWidth,
      maxHeight: maxHeight,
      format: format
    )
    var exported = ErikaFrameExportResultC()
    let status = withUnsafePointer(to: &request) { requestPtr in
      withUnsafeMutablePointer(to: &exported) { resultPtr in
        frame(session, UnsafeRawPointer(requestPtr), UnsafeMutableRawPointer(resultPtr))
      }
    }
    if status != 0 {
      let detail = currentEventMessage() ?? "ErikaStatus \(status)"
      throw ErikaPluginError.diagnosticsFailed("Frame export failed: \(detail)")
    }
    defer {
      withUnsafeMutablePointer(to: &exported) { resultPtr in
        frameExportResultFree(UnsafeMutableRawPointer(resultPtr))
      }
    }
    return frameExportMap(exported)
  }

  func closeFrameExportSession(_ session: UnsafeMutableRawPointer) {
    frameExportSessionClose?(session)
  }

  /// Whether a JSON table method takes no presenter (scope `global`), read
  /// once from the table itself so the plugin keeps no copy of it. Such a
  /// method is called with a NULL handle and without the player's native
  /// call lock: a long export must not hold up rendering.
  func isGlobalJsonMethod(_ method: String) -> Bool {
    globalJsonMethodsLock.lock()
    defer { globalJsonMethodsLock.unlock() }
    if globalJsonMethods == nil {
      var global: Set<String> = ["listMethods", "getLogs"]
      if let invokeJson,
         let response = "listMethods".withCString({ name in
           "{}".withCString { invokeJson(nil, name, $0) }
         }),
         let json = takeJsonString(response),
         let envelope = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
         let value = envelope["value"] as? [String: Any],
         let methods = value["methods"] as? [[String: Any]] {
        global = Set(methods.compactMap { entry in
          entry["scope"] as? String == "global" ? entry["name"] as? String : nil
        })
      }
      globalJsonMethods = global
    }
    return globalJsonMethods?.contains(method) ?? false
  }

  /// Copies and frees a JSON string returned by a diagnostics function.
  func takeJsonString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
    guard let pointer else {
      return nil
    }
    defer { stringFree(pointer) }
    return String(validatingUTF8: pointer)
  }

  func readLogsJson(limit: UInt32, minLevel: Int32, sinceSeq: UInt64) throws -> String {
    guard let logReadJson else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_log_read_json")
    }
    var output: UnsafeMutablePointer<CChar>?
    let status = logReadJson(limit, minLevel, sinceSeq, &output)
    guard status == 0 else {
      let detail = currentEventMessage() ?? "ErikaStatus \(status)"
      throw ErikaPluginError.diagnosticsFailed("erika_log_read_json failed: \(detail)")
    }
    guard let json = takeJsonString(output) else {
      throw ErikaPluginError.diagnosticsFailed("erika_log_read_json returned no JSON")
    }
    return json
  }

  func currentEventMessage() -> String? {
    guard let pointer = lastErrorMessage() else {
      return nil
    }
    defer { stringFree(pointer) }
    return String(validatingUTF8: pointer)
  }

  func exportGif(
    inputUri: String,
    outputPath: String,
    startMillis: UInt64,
    endMillis: UInt64,
    framesPerSecond: UInt32,
    outputWidth: UInt32,
    outputHeight: UInt32,
    quality: Int32,
    loopCount: Int32,
    overwrite: Bool,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64
  ) throws -> ErikaGifExportResultC {
    guard let exportGif else {
      throw ErikaPluginError.symbolMissing("erika_export_gif")
    }
    let names = httpHeaders.keys.map { strdup($0) }
    let values = httpHeaders.values.map { strdup($0) }
    defer {
      names.forEach { free($0) }
      values.forEach { free($0) }
    }
    let headers = zip(names, values).map { ErikaHttpHeader(name: $0.0, value: $0.1) }
    return try inputUri.withCString { inputPtr in
      try outputPath.withCString { outputPtr in
        try headers.withUnsafeBufferPointer { buffer in
          var options = ErikaGifExportOptionsC(
            inputUri: inputPtr,
            outputPath: outputPtr,
            startMillis: startMillis,
            endMillis: endMillis,
            framesPerSecond: framesPerSecond,
            outputWidth: outputWidth,
            outputHeight: outputHeight,
            quality: quality,
            loopCount: loopCount,
            overwrite: overwrite,
            headers: buffer.baseAddress.map(UnsafeRawPointer.init),
            headerCount: UInt(headers.count),
            httpReadAheadBytes: httpReadAheadBytes
          )
          var exportResult = ErikaGifExportResultC()
          let status = withUnsafePointer(to: &options) { optionsPtr in
            withUnsafeMutablePointer(to: &exportResult) { resultPtr in
              exportGif(UnsafeRawPointer(optionsPtr), UnsafeMutableRawPointer(resultPtr))
            }
          }
          if status != 0 {
            let detail = currentEventMessage() ?? "ErikaStatus \(status)"
            throw ErikaPluginError.invalidArguments("GIF export failed: \(detail)")
          }
          return exportResult
        }
      }
    }
  }
}

private final class ErikaPlayerHost {
  let id: Int64

  private let library: ErikaNativeLibrary
  private let handle: UnsafeMutableRawPointer
  private let renderQueue: DispatchQueue
  /// Diagnostics and JSON pass-through calls run here, never on the main
  /// thread: they can take as long as the work they ask for (a subtitle file
  /// parsed, a frame export decoded).
  let jsonQueue: DispatchQueue
  private let nativeCallLock = NSRecursiveLock()
  private weak var attachedView: ErikaMetalSurfaceView?
  private weak var attachedTexture: ErikaFlutterTextureSurface?
  private var attachedViewId: Int64?
  private var displayLinkDriver: ErikaDisplayLinkDriver?
  private var displayLinkDisplayID: CGDirectDisplayID?
  private var displayConfigurationObservers: [NSObjectProtocol] = []
  private var displayTimer: Timer?
  private var displayTimerFps: Double = 0.0
  private var loggedRenderThread = false
  private var startTimeSeconds: CFTimeInterval = CACurrentMediaTime()
  private var currentDanmakuConfig = ErikaDanmakuConfigC()
  private var latestPresenterStats = ErikaPresenterStatsC()
  private(set) var nowPlayingTitle = ""
  private(set) var nowPlayingArtist: String?
  private(set) var nowPlayingAlbum: String?
  private(set) var nowPlayingArtwork: MPMediaItemArtwork?
  private(set) var durationSeconds: Double?
  private(set) var positionSeconds = 0.0
  private(set) var playbackRate = 1.0
  private(set) var playbackState = 0
  private var positionUpdateTime = ProcessInfo.processInfo.systemUptime
  var onNowPlayingChanged: ((ErikaPlayerHost) -> Void)?

  var isPlaying: Bool { playbackState == 3 }

  var isStopped: Bool { playbackState == 5 || playbackState == 6 || playbackState == 7 }

  var nowPlayingPositionSeconds: Double {
    guard isPlaying else { return positionSeconds }
    return positionSeconds +
      max(0, ProcessInfo.processInfo.systemUptime - positionUpdateTime) * playbackRate
  }

  init(id: Int64, library: ErikaNativeLibrary, config: ErikaPresenterConfigC) throws {
    self.id = id
    self.library = library
    renderQueue = DispatchQueue(
      label: "dev.aimesoft.erika.render.\(id)",
      qos: .userInteractive
    )
    jsonQueue = DispatchQueue(
      label: "dev.aimesoft.erika.json.\(id)",
      qos: .userInitiated
    )
    guard let handle = library.createPresenter(config: config) else {
      throw ErikaPluginError.presenterCreateFailed
    }
    self.handle = handle
    refreshDanmakuConfigSnapshot()
  }

  deinit {
    stopDisplayDriver()
    withNativeCall {
      _ = library.detachSurface(handle)
      library.destroy(handle)
    }
  }

  private func withNativeCall<T>(_ operation: () throws -> T) rethrows -> T {
    nativeCallLock.lock()
    defer { nativeCallLock.unlock() }
    return try operation()
  }

  /// Like [withNativeCall], but gives up (nil) when a call on another queue
  /// holds the lock, instead of making the calling thread wait for it. The
  /// timers that run on the main thread use this: a long call (a big
  /// subtitle file parsed) must not freeze the app's interface, and what
  /// those timers do is repeated at the next tick anyway.
  private func tryWithNativeCall<T>(_ operation: () throws -> T) rethrows -> T? {
    guard nativeCallLock.try() else { return nil }
    defer { nativeCallLock.unlock() }
    return try operation()
  }

  func open(
    uri: String,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64 = 0,
    httpBackBufferBytes: UInt64 = 0
  ) throws {
    playbackState = 0
    positionSeconds = 0
    durationSeconds = nil
    positionUpdateTime = ProcessInfo.processInfo.systemUptime
    if nowPlayingTitle.isEmpty {
      let fallbackTitle = URL(string: uri)?.lastPathComponent.removingPercentEncoding
        ?? URL(fileURLWithPath: uri).lastPathComponent
      nowPlayingTitle = fallbackTitle.isEmpty ? "Erika" : fallbackTitle
    }
    try withNativeCall {
      try uri.withCString { cString in
        guard !httpHeaders.isEmpty || httpReadAheadBytes > 0 || httpBackBufferBytes > 0 else {
          try check(library.open(handle, cString), operation: "open")
          return
        }
        // Never silently drop the headers or the HTTP tuning request: falling
        // back to the headerless entry point turns an authenticated stream
        // into an opaque 403, and swallowing them hides a stale kernel.
        if let openWithOptions = library.openWithOptions {
          let names = httpHeaders.keys.map { strdup($0) }
          let values = httpHeaders.values.map { strdup($0) }
          defer {
            names.forEach { free($0) }
            values.forEach { free($0) }
          }
          let headers = zip(names, values).map { ErikaHttpHeader(name: $0.0, value: $0.1) }
          try headers.withUnsafeBufferPointer { buffer in
            var options = ErikaOpenOptions(
              headers: buffer.baseAddress.map(UnsafeRawPointer.init),
              headerCount: UInt(headers.count),
              httpReadAheadBytes: httpReadAheadBytes,
              httpBackBufferBytes: httpBackBufferBytes
            )
            try withUnsafePointer(to: &options) { optionsPtr in
              try check(openWithOptions(handle, cString, UnsafeRawPointer(optionsPtr)), operation: "open")
            }
          }
          return
        }
        if httpReadAheadBytes > 0 || httpBackBufferBytes > 0 {
          throw ErikaPluginError.openOptionsUnsupported
        }
        // Never fall back to the headerless entry point here: silently dropping
        // the headers turns an authenticated stream into an opaque 403.
        guard let openWithHeaders = library.openWithHeaders else {
          throw ErikaPluginError.httpHeadersUnsupported
        }
        let names = httpHeaders.keys.map { strdup($0) }
        let values = httpHeaders.values.map { strdup($0) }
        defer {
          names.forEach { free($0) }
          values.forEach { free($0) }
        }
        let headers = zip(names, values).map { ErikaHttpHeader(name: $0.0, value: $0.1) }
        try headers.withUnsafeBufferPointer { buffer in
          try check(openWithHeaders(handle, cString, buffer.baseAddress.map(UnsafeRawPointer.init), UInt(headers.count)), operation: "open")
        }
      }
    }
    onNowPlayingChanged?(self)
  }

  func play() throws {
    try withNativeCall {
      try check(library.play(handle), operation: "play")
    }
  }

  func pause() throws {
    try withNativeCall {
      try check(library.pause(handle), operation: "pause")
    }
  }

  func stop() throws {
    try withNativeCall {
      try check(library.stop(handle), operation: "stop")
    }
  }

  func close() throws {
    try withNativeCall {
      try check(library.close(handle), operation: "close")
    }
  }

  func seek(positionMicros: UInt64) throws {
    try withNativeCall {
      try check(library.seek(handle, positionMicros), operation: "seek")
    }
    positionSeconds = Double(positionMicros) / 1_000_000
    positionUpdateTime = ProcessInfo.processInfo.systemUptime
    onNowPlayingChanged?(self)
  }

  func setPlaybackRate(_ rate: Double) throws {
    guard let setRate = library.setPlaybackRate else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_playback_rate")
    }
    try withNativeCall {
      try check(setRate(handle, rate), operation: "set_playback_rate")
    }
    positionSeconds = nowPlayingPositionSeconds
    positionUpdateTime = ProcessInfo.processInfo.systemUptime
    playbackRate = rate
    onNowPlayingChanged?(self)
  }

  func setMediaMetadata(title: String, artist: String?, album: String?, artworkData: Data?) throws {
    let artwork: MPMediaItemArtwork?
    if let artworkData {
      guard let image = NSImage(data: artworkData) else {
        throw ErikaPluginError.invalidArguments("metadata.artwork must contain a supported image.")
      }
      artwork = MPMediaItemArtwork(boundsSize: image.size) { _ in image }
    } else {
      artwork = nil
    }
    nowPlayingTitle = title
    nowPlayingArtist = artist
    nowPlayingAlbum = album
    nowPlayingArtwork = artwork
    onNowPlayingChanged?(self)
  }

  func clearMediaMetadata() {
    nowPlayingTitle = ""
    nowPlayingArtist = nil
    nowPlayingAlbum = nil
    nowPlayingArtwork = nil
    onNowPlayingChanged?(self)
  }

  func setVolume(_ volume: Double) throws {
    guard let setVolume = library.setVolume else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_volume")
    }
    let clampedVolume = volume.isFinite ? min(max(volume, 0.0), 1.0) : 1.0
    try withNativeCall {
      try check(setVolume(handle, clampedVolume), operation: "set_volume")
    }
  }

  func setUpscaler(mode: Int32) throws {
    guard let setUpscaler = library.setUpscaler else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_upscaler")
    }
    try withNativeCall {
      try check(setUpscaler(handle, mode), operation: "set_upscaler")
    }
  }

  func setSubtitleScale(_ scale: Double) throws {
    guard let setSubtitleScale = library.setSubtitleScale else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_scale")
    }
    let clampedScale = scale.isFinite ? min(max(scale, 0.25), 4.0) : 1.0
    try withNativeCall {
      try check(setSubtitleScale(handle, clampedScale), operation: "set_subtitle_scale")
    }
  }

  func setSubtitleFont(family: String?, filePath: String?) throws {
    guard let setSubtitleFont = library.setSubtitleFont else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_font")
    }
    let status = withNativeCall {
      withOptionalCString(family ?? "") { familyCString in
        withOptionalCString(filePath ?? "") { filePathCString in
          setSubtitleFont(handle, familyCString, filePathCString)
        }
      }
    }
    try check(status, operation: "set_subtitle_font")
  }

  func setSubtitleStyle(
    fontFamily: String?,
    fontFilePath: String?,
    primaryRgba: UInt32,
    outlineRgba: UInt32,
    fontSize: Double,
    outlineWidth: Double,
    bold: Bool,
    italic: Bool,
    underline: Bool,
    strikeOut: Bool,
    spacing: Double,
    scaleXPercent: Double,
    scaleYPercent: Double,
    borderStyle: Int32,
    shadowDepth: Double,
    blur: Double,
    alignment: Int32,
    marginLeft: Int32,
    marginRight: Int32,
    marginVertical: Int32,
    overrideMask: UInt32
  ) throws {
    guard let setSubtitleStyle = library.setSubtitleStyle else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_style")
    }
    let status = withNativeCall {
      withOptionalCString(fontFamily ?? "") { fontFamilyCString in
        withOptionalCString(fontFilePath ?? "") { fontFilePathCString in
          var style = ErikaSubtitleStyleC(
            fontFamily: fontFamilyCString,
            fontFilePath: fontFilePathCString,
            primaryColorRgba: primaryRgba,
            outlineColorRgba: outlineRgba,
            fontSize: fontSize,
            outlineWidth: outlineWidth,
            bold: bold,
            italic: italic,
            underline: underline,
            strikeOut: strikeOut,
            spacing: spacing,
            scaleXPercent: scaleXPercent,
            scaleYPercent: scaleYPercent,
            borderStyle: borderStyle,
            shadowDepth: shadowDepth,
            blur: blur,
            alignment: alignment,
            marginLeft: marginLeft,
            marginRight: marginRight,
            marginVertical: marginVertical,
            overrideMask: overrideMask
          )
          return withUnsafePointer(to: &style) { pointer in
            setSubtitleStyle(handle, UnsafeRawPointer(pointer))
          }
        }
      }
    }
    try check(status, operation: "set_subtitle_style")
  }

  func upscalerStatus() throws -> [String: Any] {
    guard let getStatus = library.getUpscalerStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_upscaler_status")
    }
    var status = ErikaUpscalerStatusC()
    let result = withNativeCall {
      withUnsafeMutablePointer(to: &status) { pointer in
        getStatus(handle, UnsafeMutableRawPointer(pointer))
      }
    }
    try check(result, operation: "get_upscaler_status")
    return status.toFlutterMap()
  }

  func registerSubtitleMemoryFont(_ data: Data) throws -> UInt64 {
    guard let register = library.registerSubtitleMemoryFont else { throw ErikaPluginError.symbolMissing("erika_presenter_register_subtitle_memory_font") }
    var fontId: UInt64 = 0
    let status = withNativeCall {
      data.withUnsafeBytes { register(handle, $0.bindMemory(to: UInt8.self).baseAddress, UInt(data.count), &fontId) }
    }
    try check(status, operation: "register_subtitle_memory_font")
    return fontId
  }

  func selectSubtitleMemoryFonts(_ fontIds: [UInt64]) throws {
    guard let select = library.selectSubtitleMemoryFonts else { throw ErikaPluginError.symbolMissing("erika_presenter_select_subtitle_memory_fonts") }
    try withNativeCall {
      try fontIds.withUnsafeBufferPointer { try check(select(handle, $0.baseAddress, UInt($0.count)), operation: "select_subtitle_memory_fonts") }
    }
  }

  func clearSubtitleMemoryFonts() throws {
    guard let clear = library.clearSubtitleMemoryFonts else { throw ErikaPluginError.symbolMissing("erika_presenter_clear_subtitle_memory_fonts") }
    try withNativeCall {
      try check(clear(handle), operation: "clear_subtitle_memory_fonts")
    }
  }

  func subtitleMemoryFontStatus() throws -> [String: Any] {
    guard let getStatus = library.getSubtitleMemoryFontStatus else { throw ErikaPluginError.symbolMissing("erika_presenter_get_subtitle_memory_font_status") }
    var status = ErikaSubtitleMemoryFontStatusC()
    defer {
      if let freeStatus = library.freeSubtitleMemoryFontStatus { withUnsafeMutablePointer(to: &status) { freeStatus(UnsafeMutableRawPointer($0)) } }
    }
    try withNativeCall {
      try withUnsafeMutablePointer(to: &status) { try check(getStatus(handle, UnsafeMutableRawPointer($0)), operation: "get_subtitle_memory_font_status") }
    }
    return ["registeredCount": Int(status.registeredCount), "registeredBytes": Int(status.registeredBytes), "selectedCount": Int(status.selectedCount), "generation": Int64(clamping: status.generation), "selectedIds": status.selectedIds.map { pointer in (0..<Int(status.selectedCount)).map { Int64(clamping: pointer[$0]) } } ?? []]
  }

  func outputStatus() throws -> [String: Any] {
    guard let getStatus = library.getOutputStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_output_status")
    }
    var status = ErikaOutputStatusC()
    let result = withNativeCall {
      withUnsafeMutablePointer(to: &status) { pointer in
        getStatus(handle, UnsafeMutableRawPointer(pointer))
      }
    }
    try check(result, operation: "get_output_status")
    return status.toFlutterMap()
  }

  func resourceStatus() throws -> [String: Any] {
    guard let getStatus = library.getResourceStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_resource_status")
    }
    var status = ErikaPresenterResourceStatusC()
    let result = withNativeCall {
      withUnsafeMutablePointer(to: &status) { pointer in
        getStatus(handle, UnsafeMutableRawPointer(pointer))
      }
    }
    try check(result, operation: "get_resource_status")
    return status.toFlutterMap()
  }

  /// Generic pass-through into liberika's JSON method table. Returns the
  /// response envelope (`{"ok":…}`) as JSON text for Dart to decode.
  func invokeJson(method: String, argumentsJson: String) throws -> String {
    guard let invoke = library.invokeJson else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_presenter_invoke_json")
    }
    let call = { (target: UnsafeMutableRawPointer?) in
      method.withCString { methodPointer in
        argumentsJson.withCString { argumentsPointer in
          invoke(target, methodPointer, argumentsPointer)
        }
      }
    }
    // Methods that need no presenter run without the native call lock, so a
    // long one does not hold up the calls rendering makes under it.
    let response = library.isGlobalJsonMethod(method)
      ? call(nil)
      : withNativeCall { call(handle) }
    guard let json = library.takeJsonString(response) else {
      throw ErikaPluginError.diagnosticsFailed("erika_presenter_invoke_json returned no JSON")
    }
    return json
  }

  /// Diagnostics snapshot JSON (schema 1). Returned to Dart as a string and
  /// decoded there, so new fields need no plugin change.
  func diagnosticsSnapshotJson() throws -> String {
    guard let getDiagnostics = library.getDiagnosticsJson else {
      throw ErikaPluginError.diagnosticsUnsupported("erika_presenter_get_diagnostics_json")
    }
    var output: UnsafeMutablePointer<CChar>?
    let result = withNativeCall {
      getDiagnostics(handle, &output)
    }
    try check(result, operation: "get_diagnostics_json")
    guard let json = library.takeJsonString(output) else {
      throw ErikaPluginError.diagnosticsFailed("erika_presenter_get_diagnostics_json returned no JSON")
    }
    return json
  }

  func presenterStats() -> [String: Any] {
    // The last stats the render tick stored; while another queue is inside a
    // long call the previous copy is returned instead of waiting for it.
    tryWithNativeCall { latestPresenterStats.toFlutterMap() }
      ?? latestPresenterStats.toFlutterMap()
  }

  func addExternalSubtitle(uri: String) throws -> Int64 {
    var trackId: Int64 = 0
    try withNativeCall {
      try uri.withCString { cString in
        try check(
          library.addExternalSubtitle(handle, cString, &trackId),
          operation: "add_external_subtitle"
        )
      }
    }
    return trackId
  }

  func removeSubtitleTrack(trackId: Int64) throws {
    try withNativeCall {
      try check(
        library.removeSubtitleTrack(handle, trackId),
        operation: "remove_subtitle_track"
      )
    }
  }

  func loadDanmakuFile(uri: String) throws {
    guard let load = library.loadDanmakuFile else {
      throw ErikaPluginError.symbolMissing("erika_presenter_load_danmaku_file")
    }
    try withNativeCall {
      try uri.withCString { cString in
        try check(load(handle, cString), operation: "load_danmaku_file")
      }
    }
  }

  func loadDanmakuJson(_ json: String) throws {
    guard let load = library.loadDanmakuJson else {
      throw ErikaPluginError.symbolMissing("erika_presenter_load_danmaku_json")
    }
    try withNativeCall {
      try json.withCString { cString in
        try check(load(handle, cString), operation: "load_danmaku_json")
      }
    }
  }

  func addDanmakuTrackFile(uri: String, name: String?, offsetMicros: Int64) throws -> UInt64 {
    guard let add = library.addDanmakuTrackFile else {
      throw ErikaPluginError.symbolMissing("erika_presenter_add_danmaku_track_file")
    }
    var trackId: UInt64 = 0
    let status = withNativeCall {
      uri.withCString { uriCString in
        withOptionalCString(name) { nameCString in
          add(handle, uriCString, nameCString, offsetMicros, &trackId)
        }
      }
    }
    try check(status, operation: "add_danmaku_track_file")
    return trackId
  }

  func addDanmakuTrackJson(_ json: String, name: String?, offsetMicros: Int64) throws -> UInt64 {
    guard let add = library.addDanmakuTrackJson else {
      throw ErikaPluginError.symbolMissing("erika_presenter_add_danmaku_track_json")
    }
    var trackId: UInt64 = 0
    let status = withNativeCall {
      json.withCString { jsonCString in
        withOptionalCString(name) { nameCString in
          add(handle, jsonCString, nameCString, offsetMicros, &trackId)
        }
      }
    }
    try check(status, operation: "add_danmaku_track_json")
    return trackId
  }

  func removeDanmakuTrack(trackId: UInt64) throws {
    guard let remove = library.removeDanmakuTrack else {
      throw ErikaPluginError.symbolMissing("erika_presenter_remove_danmaku_track")
    }
    try withNativeCall {
      try check(remove(handle, trackId), operation: "remove_danmaku_track")
    }
  }

  func setDanmakuTrackEnabled(trackId: UInt64, enabled: Bool) throws {
    guard let setEnabled = library.setDanmakuTrackEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_track_enabled")
    }
    try withNativeCall {
      try check(setEnabled(handle, trackId, enabled), operation: "set_danmaku_track_enabled")
    }
  }

  func setDanmakuTrackOffset(trackId: UInt64, offsetMicros: Int64) throws {
    guard let setOffset = library.setDanmakuTrackOffset else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_track_offset")
    }
    try withNativeCall {
      try check(setOffset(handle, trackId, offsetMicros), operation: "set_danmaku_track_offset")
    }
  }

  func setDanmakuGlobalOffset(offsetMicros: Int64) throws {
    guard let setOffset = library.setDanmakuGlobalOffset else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_global_offset")
    }
    try withNativeCall {
      try check(setOffset(handle, offsetMicros), operation: "set_danmaku_global_offset")
    }
  }

  func danmakuTracks() throws -> [[String: Any]] {
    guard let danmakuTracks = library.danmakuTracks else {
      throw ErikaPluginError.symbolMissing("erika_presenter_danmaku_tracks")
    }
    return try withNativeCall {
      var count: Int = 0
      try check(danmakuTracks(handle, nil, 0, &count), operation: "danmaku_tracks_len")
      if count <= 0 {
        return []
      }
      var tracks = Array(repeating: ErikaDanmakuTrackInfoC(), count: count)
      var written: Int = 0
      let status = tracks.withUnsafeMutableBufferPointer { buffer in
        danmakuTracks(handle, UnsafeMutableRawPointer(buffer.baseAddress), buffer.count, &written)
      }
      try check(status, operation: "danmaku_tracks")
      let result = tracks.prefix(min(written, tracks.count)).map { $0.toFlutterMap() }
      if let free = library.freeDanmakuTrackInfo {
        for index in tracks.indices {
          withUnsafeMutablePointer(to: &tracks[index]) { pointer in
            free(UnsafeMutableRawPointer(pointer))
          }
        }
      }
      return result
    }
  }

  func clearDanmaku() throws {
    guard let clear = library.clearDanmaku else {
      throw ErikaPluginError.symbolMissing("erika_presenter_clear_danmaku")
    }
    try withNativeCall {
      try check(clear(handle), operation: "clear_danmaku")
    }
  }

  func setDanmakuEnabled(_ enabled: Bool) throws {
    guard let setEnabled = library.setDanmakuEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_enabled")
    }
    try withNativeCall {
      try check(setEnabled(handle, enabled), operation: "set_danmaku_enabled")
    }
    currentDanmakuConfig.enabled = enabled ? 1 : 0
  }

  func setDebugHudEnabled(_ enabled: Bool) throws {
    guard let setEnabled = library.setDebugHudEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_debug_hud_enabled")
    }
    try withNativeCall {
      try check(setEnabled(handle, enabled), operation: "set_debug_hud_enabled")
    }
  }

  func danmakuConfigSnapshot() -> ErikaDanmakuConfigC {
    currentDanmakuConfig
  }

  private func refreshDanmakuConfigSnapshot() {
    guard let getConfig = library.getDanmakuConfig else {
      return
    }
    var config = ErikaDanmakuConfigC()
    let status = withNativeCall {
      withUnsafeMutablePointer(to: &config) { pointer in
        getConfig(handle, UnsafeMutableRawPointer(pointer))
      }
    }
    if status == 0 {
      currentDanmakuConfig = config
    }
  }

  func setDanmakuConfig(_ config: ErikaDanmakuConfigC) throws {
    guard let setConfig = library.setDanmakuConfig else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_config_ptr")
    }
    var config = config
    let status = withNativeCall {
      withUnsafePointer(to: &config) { pointer in
        setConfig(handle, UnsafeRawPointer(pointer))
      }
    }
    try check(status, operation: "set_danmaku_config")
    currentDanmakuConfig = config
  }

  func setDanmakuFont(family: String?, filePath: String?) throws {
    guard let setFont = library.setDanmakuFont else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_font")
    }
    let family = family ?? ""
    let filePath = filePath ?? ""
    let status = withNativeCall {
      withOptionalCString(family) { familyCString in
        withOptionalCString(filePath) { filePathCString in
          setFont(handle, familyCString, filePathCString)
        }
      }
    }
    try check(status, operation: "set_danmaku_font")
    refreshDanmakuConfigSnapshot()
  }

  func setDanmakuBlockWordsJson(_ json: String) throws {
    guard let setBlockWords = library.setDanmakuBlockWords else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_block_words_json")
    }
    try withNativeCall {
      try json.withCString { cString in
        try check(setBlockWords(handle, cString), operation: "set_danmaku_block_words")
      }
    }
    refreshDanmakuConfigSnapshot()
  }

  func selectAudioTrack(trackId: Int64?) throws {
    try withNativeCall {
      try check(
        library.selectAudioTrack(handle, trackId ?? -1),
        operation: "select_audio_track"
      )
    }
  }

  func selectSubtitleTrack(trackId: Int64?) throws {
    try withNativeCall {
      try check(
        library.selectSubtitleTrack(handle, trackId ?? -1),
        operation: "select_subtitle_track"
      )
    }
  }

  func tracks() throws -> [[String: Any]] {
    return try withNativeCall {
      var count: Int = 0
      try check(
        library.tracks(handle, nil, 0, &count),
        operation: "tracks_len"
      )
      if count <= 0 {
        return []
      }

      var tracks = Array(repeating: ErikaTrackInfoC(), count: count)
      var written: Int = 0
      let status = tracks.withUnsafeMutableBufferPointer { buffer in
        library.tracks(handle, UnsafeMutableRawPointer(buffer.baseAddress), buffer.count, &written)
      }
      try check(status, operation: "tracks")
      let result = tracks.prefix(min(written, tracks.count)).map { $0.toFlutterMap() }
      for index in tracks.indices {
        withUnsafeMutablePointer(to: &tracks[index]) { pointer in
          library.freeTrackInfo(UnsafeMutableRawPointer(pointer))
        }
      }
      return result
    }
  }

  func trackSelection() throws -> [String: Any] {
    var selection = ErikaTrackSelectionC()
    let status = withNativeCall {
      withUnsafeMutablePointer(to: &selection) { pointer in
        library.trackSelection(handle, UnsafeMutableRawPointer(pointer))
      }
    }
    try check(status, operation: "track_selection")
    return selection.toFlutterMap()
  }

  func captureFrameRgba(width: Int, height: Int) -> Data? {
    guard width > 0, height > 0, let captureFrameRgba = library.captureFrameRgba else {
      return nil
    }
    let byteCount = width * height * 4
    var data = Data(count: byteCount)
    let status = withNativeCall {
      data.withUnsafeMutableBytes { buffer in
        captureFrameRgba(
          handle,
          UInt32(width),
          UInt32(height),
          buffer.baseAddress,
          byteCount
        )
      }
    }
    guard status == 0 else {
      NSLog("Erika: captureFrameRgba failed with status \(status)")
      return nil
    }
    return data
  }

  func screenshot(view: ErikaMetalSurfaceView? = nil, width: Int? = nil, height: Int? = nil) -> Data? {
    if let width, let height, let data = captureFrameRgba(width: width, height: height) {
      return data
    }
    return (view ?? attachedView)?.pngSnapshotData()
  }

  func attach(view: ErikaMetalSurfaceView) throws {
    // Moving the surface between windows re-attaches an already running
    // player. Only a genuinely new attachment restarts the host clock; a
    // migration must not rewind the timeline the engine is already on.
    let isReattach = attachedView != nil || attachedTexture != nil
    attachedTexture?.attachedPlayer = nil
    attachedTexture = nil
    attachedView = view
    attachedViewId = view.platformViewId
    view.attachedPlayerId = id
    erikaWindowOverlayTrace(
      "attach player=\(id) surface=\(view.platformViewId) " +
      "window=\((view as? NSView)?.window?.windowNumber ?? -1) " +
      "drawable=\(view.metalLayer.drawableSize) reattach=\(isReattach)"
    )
    try attachOrResize(view: view, attach: true)
    startDisplayDriverIfNeeded(resetClock: !isReattach)
  }

  func attach(texture: ErikaFlutterTextureSurface) throws {
    let isReattach = attachedView != nil || attachedTexture != nil
    attachedView?.attachedPlayerId = nil
    attachedView = nil
    attachedTexture?.attachedPlayer = nil
    attachedTexture = texture
    attachedViewId = texture.textureId
    texture.attachedPlayer = self
    let metrics = texture.metrics()
    try withNativeCall {
      try check(
        library.attachFlutterTexture(
          handle,
          1, // ErikaFlutterTextureKind_MacOsTextureRegistrar
          texture.textureId,
          metrics.width,
          metrics.height,
          metrics.scale
        ),
        operation: "attach_flutter_texture"
      )
    }
    startDisplayDriverIfNeeded(resetClock: !isReattach)
  }

  func detach(viewId: Int64?) {
    guard viewId == nil || attachedViewId == viewId else {
      return
    }
    attachedView?.attachedPlayerId = nil
    attachedTexture?.attachedPlayer = nil
    attachedView = nil
    attachedTexture = nil
    attachedViewId = nil
    stopDisplayDriver()
    withNativeCall {
      _ = library.detachSurface(handle)
    }
  }

  func resizeFromAttachedView() {
    guard let view = attachedView else {
      return
    }
    do {
      try attachOrResize(view: view, attach: false)
      startDisplayDriverIfNeeded(resetClock: false)
    } catch {
      NSLog("ErikaFlutterPlugin: resize failed: \(error)")
    }
  }

  func resizeFromAttachedTexture() {
    guard let texture = attachedTexture else {
      return
    }
    let metrics = texture.metrics()
    do {
      try withNativeCall {
        try check(
          library.resizeSurface(handle, metrics.width, metrics.height, metrics.scale),
          operation: "resize_flutter_texture"
        )
      }
      startDisplayDriverIfNeeded(resetClock: false)
    } catch {
      NSLog("ErikaFlutterPlugin: texture resize failed: \(error)")
    }
  }

  func renderTick() {
    if !loggedRenderThread {
      loggedRenderThread = true
      erikaWindowOverlayTrace(
        "render driver player=\(id) mainThread=\(Thread.isMainThread)"
      )
    }
    let timeSeconds = CACurrentMediaTime() - startTimeSeconds
    let textureSurface = attachedTexture
    let preparedFrame = textureSurface?.prepareFrame()
    if textureSurface != nil && preparedFrame == nil {
      return
    }
    let previousStats = latestPresenterStats
    var stats = ErikaPresenterStatsC()
    let tickStatus = tryWithNativeCall { () -> Int32 in
      if let preparedFrame {
        let rawTexture = UInt64(UInt(bitPattern: Unmanaged.passUnretained(
          preparedFrame.metalTexture as AnyObject
        ).toOpaque()))
        let bufferStatus = library.setFlutterTextureBuffer(
          handle,
          rawTexture,
          preparedFrame.width,
          preparedFrame.height
        )
        if bufferStatus != 0 {
          return bufferStatus
        }
      }
      let status = withUnsafeMutablePointer(to: &stats) { pointer in
        library.renderTick(handle, timeSeconds, UnsafeMutableRawPointer(pointer))
      }
      if status == 0 {
        latestPresenterStats = stats
      }
      return status
    }
    // Nil: another queue is inside a long call on this player; this tick is
    // skipped and the next one renders.
    guard let status = tickStatus else {
      return
    }
    if status != 0 {
      NSLog("ErikaFlutterPlugin: render_tick failed with status \(status)")
    } else if let textureSurface, let preparedFrame {
      let renderedThisTick =
        stats.renderedVideoFrames > previousStats.renderedVideoFrames ||
        stats.renderedTestFrames > previousStats.renderedTestFrames ||
        stats.overlayFrames > previousStats.overlayFrames ||
        stats.danmakuFrames > previousStats.danmakuFrames
      guard renderedThisTick else {
        return
      }
      textureSurface.publish(preparedFrame)
    }
  }

  func pollEvents(sendEvent: (([String: Any]) -> Void)?) {
    // Skipped while another queue is inside a long call on this player; the
    // events wait in the engine and the next poll takes them.
    _ = tryWithNativeCall {
      while true {
        var event = ErikaEventC()
        let status = withUnsafeMutablePointer(to: &event) { pointer in
          library.pollEvent(handle, UnsafeMutableRawPointer(pointer))
        }
        if status == 0 {
          if event.kind == 1 {
            positionSeconds = nowPlayingPositionSeconds
            positionUpdateTime = ProcessInfo.processInfo.systemUptime
            playbackState = Int(event.state)
            if isStopped {
              positionSeconds = 0
            }
            if playbackState == 6 {
              durationSeconds = nil
            }
          } else if event.kind == 2 {
            durationSeconds = event.durationMicros >= 0
              ? Double(event.durationMicros) / 1_000_000
              : nil
          } else if event.kind == 3 {
            positionSeconds = Double(event.positionMicros) / 1_000_000
            positionUpdateTime = ProcessInfo.processInfo.systemUptime
          }
          if event.kind == 1 || event.kind == 2 || event.kind == 3 {
            onNowPlayingChanged?(self)
          }
          let message = event.kind == 9 || event.kind == 11 || event.kind == 12
            ? library.currentEventMessage()
            : nil
          sendEvent?(event.toFlutterMap(playerId: id, host: self, structuredMessage: message))
          continue
        }
        if status != 5 {
          NSLog("ErikaFlutterPlugin: poll_event failed with status \(status)")
        }
        break
      }
    }
  }

  private func attachOrResize(view: ErikaMetalSurfaceView, attach: Bool) throws {
    view.updateDrawableSize()
    let width = UInt32(max(1.0, view.metalLayer.drawableSize.width).rounded())
    let height = UInt32(max(1.0, view.metalLayer.drawableSize.height).rounded())
    let scale = view.currentBackingScale
    if attach {
      let rawLayer = UInt64(UInt(bitPattern: Unmanaged.passUnretained(view.metalLayer).toOpaque()))
      try withNativeCall {
        try check(
          library.attachMetalLayer(handle, rawLayer, width, height, scale),
          operation: "attach_metal_layer"
        )
      }
      reportDisplayHeadroom()
    } else {
      try withNativeCall {
        try check(
          library.resizeSurface(handle, width, height, scale),
          operation: "resize_surface"
        )
      }
    }
  }

  /// Publishes the presenting display's EDR headroom to the renderer.
  ///
  /// The renderer negotiates its output mode on the render thread (the
  /// CVDisplayLink callback), where AppKit must not be touched, so the value
  /// is resolved here and pushed through the C ABI instead. Main thread only:
  /// it reads the attached `NSView`'s screen.
  private func reportDisplayHeadroom() {
    guard let setOutputHeadroom = library.setOutputHeadroom,
          let view = attachedView else {
      return
    }
    let headroom = view.displayEdrHeadroom()
    withNativeCall {
      _ = setOutputHeadroom(handle, headroom, true)
    }
  }

  private func startDisplayDriverIfNeeded(resetClock: Bool) {
    if resetClock {
      startTimeSeconds = CACurrentMediaTime()
    }

    // Keep the explicit FPS override as a diagnostic escape hatch. Normal
    // playback follows the actual display refresh through CVDisplayLink.
    if ProcessInfo.processInfo.environment["ERIKA_FLUTTER_TARGET_FPS"] == nil {
      let displayID = resolvedDisplayID()
      if displayLinkDriver != nil && displayLinkDisplayID == displayID {
        return
      }

      stopDisplayDriver()
      let driver = ErikaDisplayLinkDriver(displayID: displayID) { [weak self] in
        guard let self else {
          return
        }
        self.renderTick()
      }
      if let driver, driver.start() {
        displayLinkDriver = driver
        displayLinkDisplayID = displayID
        observeDisplayConfigurationChanges()
        return
      }
    }

    startFallbackDisplayTimerIfNeeded()
  }

  /// Re-targets the display link when the window changes screen, or when the
  /// screen layout itself changes.
  ///
  /// A `CVDisplayLink` is bound to the display it was created for. Without
  /// this, dragging the window to a second screen keeps it ticking at the old
  /// refresh rate, and unplugging that screen stops the callbacks entirely —
  /// with the fallback timer already torn down, rendering would simply stop.
  private func observeDisplayConfigurationChanges() {
    guard displayConfigurationObservers.isEmpty else {
      return
    }
    let center = NotificationCenter.default
    for name in [
      NSWindow.didChangeScreenNotification,
      NSApplication.didChangeScreenParametersNotification,
    ] {
      let observer = center.addObserver(
        forName: name,
        object: nil,
        queue: .main
      ) { [weak self] _ in
        // Screen parameters change without the display ID changing (the EDR
        // capability of the same screen can move), so republish the headroom
        // before the display-link retarget short-circuits.
        self?.reportDisplayHeadroom()
        self?.retargetDisplayDriverIfScreenChanged()
      }
      displayConfigurationObservers.append(observer)
    }
  }

  private func retargetDisplayDriverIfScreenChanged() {
    guard displayLinkDriver != nil else {
      return
    }
    let displayID = resolvedDisplayID()
    guard displayID != displayLinkDisplayID else {
      return
    }
    // Rebuild on the new display; never reset the host clock, this is not a
    // new playback session.
    startDisplayDriverIfNeeded(resetClock: false)
  }

  private func stopObservingDisplayConfigurationChanges() {
    for observer in displayConfigurationObservers {
      NotificationCenter.default.removeObserver(observer)
    }
    displayConfigurationObservers.removeAll()
  }

  private func startFallbackDisplayTimerIfNeeded() {
    let targetFps = resolvedDisplayTimerFps()
    if displayTimer != nil && abs(displayTimerFps - targetFps) <= erikaDisplayFpsEpsilon {
      return
    }
    stopDisplayDriver()
    let timer = Timer(timeInterval: 1.0 / targetFps, repeats: true) { [weak self] _ in
      guard let self else {
        return
      }
      self.renderQueue.async { [weak self] in
        self?.renderTick()
      }
    }
    displayTimer = timer
    displayTimerFps = targetFps
    RunLoop.main.add(timer, forMode: .common)
  }

  private func stopDisplayDriver() {
    stopObservingDisplayConfigurationChanges()
    displayLinkDriver?.invalidate()
    displayLinkDriver = nil
    displayLinkDisplayID = nil
    displayTimer?.invalidate()
    displayTimer = nil
    displayTimerFps = 0.0
  }

  private func resolvedDisplayID() -> CGDirectDisplayID {
    let screen = (attachedView as? NSView)?.window?.screen ?? NSScreen.main
    let screenNumberKey = NSDeviceDescriptionKey("NSScreenNumber")
    if let number = screen?.deviceDescription[screenNumberKey] as? NSNumber {
      return CGDirectDisplayID(number.uint32Value)
    }
    return CGMainDisplayID()
  }

  private func resolvedDisplayTimerFps() -> Double {
    if let override = ProcessInfo.processInfo.environment["ERIKA_FLUTTER_TARGET_FPS"],
       let fps = Double(override), fps.isFinite, fps > 0.0 {
      return min(max(fps, 1.0), 1000.0)
    }
    let screen = (attachedView as? NSView)?.window?.screen ?? NSScreen.main
    if #available(macOS 12.0, *), let screen = screen {
      let fps = Double(screen.maximumFramesPerSecond)
      if fps.isFinite && fps > 0.0 {
        return fps
      }
    }
    return erikaDefaultDisplayFps
  }

  private func check(_ status: Int32, operation: String) throws {
    if status != 0 {
      throw ErikaPluginError.erikaStatus(operation, status)
    }
  }
}

private extension ErikaEventC {
  func toFlutterMap(
    playerId: Int64,
    host: ErikaPlayerHost? = nil,
    structuredMessage: String? = nil
  ) -> [String: Any] {
    var map: [String: Any] = [
      "playerId": playerId,
      "kind": Int(kind),
      "status": Int(status),
      "state": Int(state),
      "durationMicros": Int(durationMicros),
      "positionMicros": Int64(positionMicros),
      "buffering": buffering != 0,
      "video": [
        "width": Int(video.width),
        "height": Int(video.height),
        "primaries": Int(video.primaries),
        "transfer": Int(video.transfer),
      ],
      "tracks": [
        "video": Int(tracks.video),
        "audio": Int(tracks.audio),
        "subtitle": Int(tracks.subtitle),
      ],
    ]
    if kind == 4 || kind == 10 {
      map["trackList"] = (try? host?.tracks()) ?? []
      map["trackSelection"] = (try? host?.trackSelection()) ?? [
        "video": -1,
        "audio": -1,
        "subtitle": -1,
      ]
    }
    if let structuredMessage,
       let data = structuredMessage.data(using: .utf8),
       let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
      if kind == 11 {
        map["decoder"] = payload
      } else if kind == 12 {
        map["audio"] = payload
      } else if kind == 9 {
        map["error"] = structuredMessage
      }
    }
    return map
  }
}

private extension ErikaTrackSelectionC {
  func toFlutterMap() -> [String: Any] {
    [
      "video": Int(video),
      "audio": Int(audio),
      "subtitle": Int(subtitle),
    ]
  }
}

private extension ErikaUpscalerStatusC {
  func toFlutterMap() -> [String: Any] {
    [
      "requestedMode": Int(requestedMode),
      "activeBackend": Int(activeBackend),
      "fallbackCount": Int64(clamping: fallbackCount),
      "upscaledFrames": Int64(clamping: upscaledFrames),
      "lastEncodeMicros": Int64(clamping: lastEncodeMicros),
      "lastGpuMicros": Int64(clamping: lastGpuMicros),
    ]
  }
}

private extension ErikaOutputStatusC {
  func toFlutterMap() -> [String: Any] {
    [
      "requestedMode": Int(requestedMode),
      "activeEncoding": Int(activeEncoding),
      "surfaceFormat": Int(surfaceFormat),
      "nativeDataSpace": Int(nativeDataSpace),
      "requestedHeadroom": Double(requestedHeadroom),
      "activeHeadroom": Double(activeHeadroom),
      "activeHeadroomKnown": activeHeadroomKnown,
      "extendedLinearActive": extendedLinearActive,
      "fallbackReason": Int(fallbackReason),
      "fallbackCount": Int64(clamping: fallbackCount),
      "dataSpaceFailures": Int64(clamping: dataSpaceFailures),
      "headroomUpdates": Int64(clamping: headroomUpdates),
      "extendedLinearFrames": Int64(clamping: extendedLinearFrames),
    ]
  }
}

private extension ErikaPresenterResourceStatusC {
  func toFlutterMap() -> [String: Any] {
    [
      "deviceCurrentAllocatedBytes": Int64(clamping: deviceCurrentAllocatedBytes),
      "deviceRecommendedWorkingSetBytes": Int64(clamping: deviceRecommendedWorkingSetBytes),
      "drawableEstimatedBytes": Int64(clamping: drawableEstimatedBytes),
      "videoFrameBytes": Int64(clamping: videoFrameBytes),
      "overlayAtlasBytes": Int64(clamping: overlayAtlasBytes),
      "danmakuAtlasBytes": Int64(clamping: danmakuAtlasBytes),
      "danmakuVertexBufferBytes": Int64(clamping: danmakuVertexBufferBytes),
      "upscalerBytes": Int64(clamping: upscalerBytes),
      "rendererTrackedBytes": Int64(clamping: rendererTrackedBytes),
      "presenterCpuDanmakuAtlasBytes": Int64(clamping: presenterCpuDanmakuAtlasBytes),
      "drawableCount": Int(drawableCount),
      "outputModeSwitches": Int64(clamping: outputModeSwitches),
    ]
  }
}

private extension ErikaPresenterStatsC {
  func toFlutterMap() -> [String: Any] {
    [
      "decodedVideoFrames": Int64(clamping: decodedVideoFrames),
      "renderedVideoFrames": Int64(clamping: renderedVideoFrames),
      "renderedTestFrames": Int64(clamping: renderedTestFrames),
      "pushedAudioFrames": Int64(clamping: pushedAudioFrames),
      "overlayFrames": Int64(clamping: overlayFrames),
      "danmakuFrames": Int64(clamping: danmakuFrames),
      "danmakuItems": Int64(clamping: danmakuItems),
      "importFailures": Int64(clamping: importFailures),
      "renderFailures": Int64(clamping: renderFailures),
      "audioFailures": Int64(clamping: audioFailures),
      "softwareVideoFrames": Int64(clamping: softwareVideoFrames),
      "hardwareVideoFrames": Int64(clamping: hardwareVideoFrames),
      "zeroCopyVideoFrames": Int64(clamping: zeroCopyVideoFrames),
      "cpuVideoFrameFallbacks": Int64(clamping: cpuVideoFrameFallbacks),
      "lastRenderMicros": Int64(clamping: lastRenderMicros),
      "lastRenderCurrentMicros": Int64(clamping: lastRenderCurrentMicros),
      "audioClockReadFrames": Int64(clamping: audioClockReadFrames),
      "audioClockQueuedFrames": Int64(clamping: audioClockQueuedFrames),
      "audioClockUnderflowFrames": Int64(clamping: audioClockUnderflowFrames),
      "audioRecoveryState": Int(audioRecoveryState),
      "audioLastErrorCode": Int(audioLastErrorCode),
      "audioRecoveryAttempts": Int64(clamping: audioRecoveryAttempts),
      "audioRecoveryCount": Int64(clamping: audioRecoveryCount),
      "audioRecoveryFailures": Int64(clamping: audioRecoveryFailures),
      "directZeroCopyVideoFrames": Int64(clamping: directZeroCopyVideoFrames),
      "sharedHandleVideoFrames": Int64(clamping: sharedHandleVideoFrames),
      "hdrSourceFrames": Int64(clamping: hdrSourceFrames),
      "hdr10OutputFrames": Int64(clamping: hdr10OutputFrames),
      "sdrTonemapFrames": Int64(clamping: sdrTonemapFrames),
      "hdr10MetadataUpdates": Int64(clamping: hdr10MetadataUpdates),
      "hdr10MetadataFailures": Int64(clamping: hdr10MetadataFailures),
      "hdr10OutputFailures": Int64(clamping: hdr10OutputFailures),
      "hdr10OutputActive": hdr10OutputActive,
      "videoFrameBackpressureDrops": Int64(clamping: videoFrameBackpressureDrops),
    ]
  }
}

private extension ErikaTrackInfoC {
  func toFlutterMap() -> [String: Any] {
    [
      "id": Int(id),
      "kind": Int(kind),
      "source": Int(source),
      "selected": selected != 0,
      "canRemove": canRemove != 0,
      "title": title.map { String(cString: $0) } as Any,
      "language": language.map { String(cString: $0) } as Any,
      "codec": codec.map { String(cString: $0) } as Any,
      "width": Int(width),
      "height": Int(height),
      "sampleRate": Int(sampleRate),
      "channels": Int(channels),
      "pixelFormat": pixelFormat.map { String(cString: $0) } as Any,
      "sampleFormat": sampleFormat.map { String(cString: $0) } as Any,
      "profile": profile.map { String(cString: $0) } as Any,
      "level": Int(level),
      "bitRate": Int64(clamping: bitRate),
      "frameRateNumerator": Int(frameRateNumerator),
      "frameRateDenominator": Int(frameRateDenominator),
    ]
  }
}

private extension ErikaDanmakuTrackInfoC {
  func toFlutterMap() -> [String: Any] {
    [
      "id": Int64(clamping: id),
      "enabled": enabled != 0,
      "offsetMicros": offsetMicros,
      "itemCount": itemCount,
      "name": name.map { String(cString: $0) } as Any,
      "source": source.map { String(cString: $0) } as Any,
    ]
  }
}

private func withOptionalCString<R>(_ value: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
  guard let value, !value.isEmpty else {
    return body(nil)
  }
  return value.withCString { pointer in
    body(pointer)
  }
}

/// Potential EDR headroom of a screen: what it can do regardless of the
/// current brightness setting, so playback does not flip between SDR and EDR
/// while the brightness slider moves. 1.0 means no EDR.
///
/// Main thread only — `NSScreen` is AppKit.
private func screenPotentialEdrHeadroom(_ screen: NSScreen?) -> Float {
  guard let screen else {
    return 1.0
  }
  let key = "maximumPotentialExtendedDynamicRangeColorComponentValue"
  guard screen.responds(to: Selector((key))),
        let number = screen.value(forKey: key) as? NSNumber else {
    return 1.0
  }
  return max(1.0, number.floatValue)
}

private protocol ErikaMetalSurfaceView: AnyObject {
  var platformViewId: Int64 { get }
  var metalLayer: CAMetalLayer { get }
  var attachedPlayerId: Int64? { get set }
  var bounds: NSRect { get }
  var currentBackingScale: Double { get }

  func updateDrawableSize()
  func pngSnapshotData() -> Data?

  /// EDR headroom of the display this view is presented on. Main thread only:
  /// the renderer consumes the reported value on its own thread and must not
  /// query AppKit itself.
  func displayEdrHeadroom() -> Float
}

private final class WeakErikaVideoPlatformViewBox {
  weak var view: (NSView & ErikaMetalSurfaceView)?

  init(view: NSView & ErikaMetalSurfaceView) {
    self.view = view
  }
}

final class ErikaVideoPlatformView: NSView, ErikaMetalSurfaceView {
  let platformViewId: Int64
  let metalLayer: CAMetalLayer

  weak var plugin: ErikaFlutterPlugin?
  var attachedPlayerId: Int64?

  var currentBackingScale: Double {
    let scale = window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 1.0
    return Double(max(1.0, scale))
  }

  init(viewId: Int64, arguments: Any?, plugin: ErikaFlutterPlugin?) {
    platformViewId = viewId
    self.plugin = plugin
    metalLayer = CAMetalLayer()
    super.init(frame: .zero)

    wantsLayer = true
    metalLayer.pixelFormat = .bgra8Unorm
    metalLayer.framebufferOnly = true
    let params = arguments as? [String: Any]
    let alphaVideo = (params?["videoAlphaMode"] as? NSNumber)?.intValue != 0
    metalLayer.isOpaque = !alphaVideo
    metalLayer.backgroundColor = alphaVideo
      ? NSColor.clear.cgColor
      : NSColor.black.cgColor
    if params?["blendMode"] as? String == "overlay" {
      metalLayer.compositingFilter = "overlayBlendMode"
    }
    if let opacity = (params?["opacity"] as? NSNumber)?.doubleValue {
      metalLayer.opacity = Float(min(max(opacity, 0.0), 1.0))
    }
    layer = metalLayer
    layerContentsRedrawPolicy = .duringViewResize
    autoresizingMask = [.width, .height]

    if let params,
       let debugLabel = params["debugLabel"] as? String,
       !debugLabel.isEmpty,
       ProcessInfo.processInfo.environment["ERIKA_DEBUG_LABELS"] == "1" {
      let label = NSTextField(labelWithString: debugLabel)
      label.textColor = NSColor(white: 1.0, alpha: 0.4)
      label.font = NSFont.systemFont(ofSize: 12, weight: .medium)
      label.translatesAutoresizingMaskIntoConstraints = false
      addSubview(label)
      NSLayoutConstraint.activate([
        label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
        label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -10),
      ])
    }
  }

  required init?(coder: NSCoder) {
    fatalError("init(coder:) has not been implemented")
  }

  deinit {
    plugin?.unregisterView(viewId: platformViewId)
  }

  override func layout() {
    super.layout()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func viewDidMoveToWindow() {
    super.viewDidMoveToWindow()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func viewDidChangeBackingProperties() {
    super.viewDidChangeBackingProperties()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  func updateDrawableSize() {
    let scale = CGFloat(currentBackingScale)
    let width = max(1.0, bounds.width * scale)
    let height = max(1.0, bounds.height * scale)
    metalLayer.frame = bounds
    metalLayer.drawableSize = CGSize(width: width, height: height)
  }

  func pngSnapshotData() -> Data? {
    snapshotPngData(of: self)
  }

  func displayEdrHeadroom() -> Float {
    screenPotentialEdrHeadroom(window?.screen)
  }
}

final class ErikaWindowOverlayView: NSView, ErikaMetalSurfaceView {
  let platformViewId: Int64 = erikaWindowHostedVideoSurfaceId
  let metalLayer: CAMetalLayer

  weak var plugin: ErikaFlutterPlugin?
  var attachedPlayerId: Int64?

  private var overlayFrameGeneration: Int64?
  private var lastTraceSignature: String?

  /// Generation of the widget that currently owns this shared overlay surface.
  /// Used to reject stale detach calls from disposed widgets.
  var activeGeneration: Int64? { overlayFrameGeneration }

  var currentBackingScale: Double {
    let scale = window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 1.0
    return Double(max(1.0, scale))
  }

  init(plugin: ErikaFlutterPlugin?) {
    self.plugin = plugin
    metalLayer = CAMetalLayer()
    super.init(frame: .zero)

    wantsLayer = true
    metalLayer.pixelFormat = .bgra8Unorm
    metalLayer.framebufferOnly = true
    metalLayer.isOpaque = true
    metalLayer.backgroundColor = NSColor.black.cgColor
    layer = metalLayer
    layerContentsRedrawPolicy = .duringViewResize
    autoresizingMask = [.width, .height]
    isHidden = true
    layer?.actions = [
      "bounds": NSNull(),
      "frame": NSNull(),
      "position": NSNull(),
    ]
  }

  required init?(coder: NSCoder) {
    fatalError("init(coder:) has not been implemented")
  }

  deinit {
    plugin?.detachOverlayView(self)
  }

  override func hitTest(_ point: NSPoint) -> NSView? {
    nil
  }

  override func layout() {
    super.layout()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func viewDidMoveToWindow() {
    super.viewDidMoveToWindow()
    erikaWindowOverlayTrace(
      "surface moved window=\(window?.windowNumber ?? -1) frame=\(frame)"
    )
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func viewDidChangeBackingProperties() {
    super.viewDidChangeBackingProperties()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  func updateOverlayFrame(_ frame: CGRect?, visible: Bool, debugLabel: String?, generation: Int64?) {
    if visible {
      overlayFrameGeneration = generation
    } else if let generation,
              let overlayFrameGeneration,
              generation != overlayFrameGeneration {
      return
    }

    toolTip = erikaDebugLabelsEnabled ? debugLabel : nil
    let shouldShow = visible &&
      (frame?.width ?? 0) > 0 &&
      (frame?.height ?? 0) > 0
    if erikaWindowOverlayTraceEnabled {
      let signature =
        "\(window?.windowNumber ?? -1)|\(visible)|\(generation ?? -1)|\(String(describing: frame))"
      if signature != lastTraceSignature {
        lastTraceSignature = signature
        erikaWindowOverlayTrace(
          "frame window=\(window?.windowNumber ?? -1) visible=\(visible) " +
          "generation=\(generation ?? -1) requested=\(String(describing: frame))"
        )
      }
    }

    CATransaction.begin()
    CATransaction.setDisableActions(true)
    defer { CATransaction.commit() }

    guard shouldShow, let frame else {
      isHidden = true
      return
    }

    let resolvedFrame = frame.integral
    if self.frame != resolvedFrame {
      self.frame = resolvedFrame
    }
    isHidden = false
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  func updateDrawableSize() {
    let scale = CGFloat(currentBackingScale)
    let width = max(1.0, bounds.width * scale)
    let height = max(1.0, bounds.height * scale)
    metalLayer.frame = bounds
    metalLayer.drawableSize = CGSize(width: width, height: height)
  }

  func pngSnapshotData() -> Data? {
    snapshotPngData(of: self)
  }

  func displayEdrHeadroom() -> Float {
    screenPotentialEdrHeadroom(window?.screen)
  }
}

private func snapshotPngData(of view: NSView) -> Data? {
  guard view.bounds.width > 0, view.bounds.height > 0 else {
    return nil
  }
  guard let representation = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
    return nil
  }
  view.cacheDisplay(in: view.bounds, to: representation)
  return representation.representation(using: .png, properties: [:])
}

final class ErikaVideoViewFactory: NSObject, FlutterPlatformViewFactory {
  private weak var plugin: ErikaFlutterPlugin?

  init(plugin: ErikaFlutterPlugin) {
    self.plugin = plugin
    super.init()
  }

  func createArgsCodec() -> (FlutterMessageCodec & NSObjectProtocol)? {
    FlutterStandardMessageCodec.sharedInstance()
  }

  func create(withViewIdentifier viewId: Int64, arguments args: Any?) -> NSView {
    let view = ErikaVideoPlatformView(viewId: viewId, arguments: args, plugin: plugin)
    plugin?.registerView(view, viewId: viewId)
    return view
  }
}

private enum ErikaAssociatedObjectKeys {
  static var windowOverlayView: UInt8 = 0
}

private extension NSWindow {
  var erikaWindowOverlayView: ErikaWindowOverlayView? {
    get {
      objc_getAssociatedObject(
        self,
        &ErikaAssociatedObjectKeys.windowOverlayView
      ) as? ErikaWindowOverlayView
    }
    set {
      objc_setAssociatedObject(
        self,
        &ErikaAssociatedObjectKeys.windowOverlayView,
        newValue,
        .OBJC_ASSOCIATION_RETAIN_NONATOMIC
      )
    }
  }
}

/// One frame export session of the plugin. Every call on the native session
/// runs on `queue`, so the session is used by one thread at a time and calls
/// come out in the order they were made.
private final class ErikaFrameExportSessionBox {
  let id: Int64
  let queue: DispatchQueue
  private var library: ErikaNativeLibrary?
  private var session: UnsafeMutableRawPointer?

  init(id: Int64) {
    self.id = id
    queue = DispatchQueue(
      label: "dev.aimesoft.erika.frame-export.\(id)",
      qos: .userInitiated
    )
  }

  deinit {
    if let library, let session {
      library.closeFrameExportSession(session)
    }
  }

  func open(
    library: ErikaNativeLibrary,
    inputUri: String,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64
  ) throws {
    session = try library.openFrameExportSession(
      inputUri: inputUri,
      httpHeaders: httpHeaders,
      httpReadAheadBytes: httpReadAheadBytes
    )
    self.library = library
  }

  func frame(
    positionMillis: UInt64,
    maxWidth: UInt32,
    maxHeight: UInt32,
    format: Int32
  ) throws -> [String: Any] {
    guard let library, let session else {
      throw ErikaPluginError.invalidArguments("Frame export session \(id) is closed.")
    }
    return try library.exportSessionFrame(
      session: session,
      positionMillis: positionMillis,
      maxWidth: maxWidth,
      maxHeight: maxHeight,
      format: format
    )
  }

  func close() {
    if let library, let session {
      library.closeFrameExportSession(session)
    }
    session = nil
  }
}

public final class ErikaFlutterPlugin: NSObject, FlutterPlugin, FlutterStreamHandler {
  static var sharedEventSink: FlutterEventSink?

  private static let playerChannelName = "erika_flutter/player"
  private static let eventsChannelName = "erika_flutter/events"
  private static let videoViewType = "erika_flutter/video_view"

  private var players: [Int64: ErikaPlayerHost] = [:]
  private var views: [Int64: WeakErikaVideoPlatformViewBox] = [:]
  private let textureRegistry: FlutterTextureRegistry
  private var textures: [Int64: ErikaFlutterTextureSurface] = [:]
  private weak var flutterHostView: NSView?
  private weak var flutterHostViewController: NSViewController?
  private var requestedFlutterViewIdentifier: Int64?
  private var requestedSecondaryWindow = false
  private var windowOverlayPlayerIds: Set<Int64> = []
  private var nextPlayerId: Int64 = 1
  private let logQueue = DispatchQueue(label: "dev.aimesoft.erika.logs", qos: .utility)
  private var frameExportSessions: [Int64: ErikaFrameExportSessionBox] = [:]
  private var nextFrameExportSessionId: Int64 = 1
  private var pollTimer: Timer?
  private var activePlayerId: Int64?
  private var remoteCommandTargets: [(MPRemoteCommand, Any)] = []
  private var systemMediaNavigation: [Int64: (previousEnabled: Bool, nextEnabled: Bool)] = [:]

  init(
    flutterHostView: NSView?,
    flutterHostViewController: NSViewController?,
    textureRegistry: FlutterTextureRegistry
  ) {
    self.flutterHostView = flutterHostView
    self.flutterHostViewController = flutterHostViewController
    self.textureRegistry = textureRegistry
    super.init()
  }

  deinit {
    pollTimer?.invalidate()
    clearNowPlayingInfo()
    remoteCommandTargets.forEach { command, target in
      command.isEnabled = false
      command.removeTarget(target)
    }
    for textureId in textures.keys {
      textureRegistry.unregisterTexture(textureId)
    }
  }

  public static func register(with registrar: FlutterPluginRegistrar) {
    let instance = ErikaFlutterPlugin(
      flutterHostView: registrar.view,
      flutterHostViewController: registrar.viewController,
      textureRegistry: registrar.textures
    )
    instance.configureSystemPlayback()
    let playerChannel = FlutterMethodChannel(
      name: playerChannelName,
      binaryMessenger: registrar.messenger
    )
    let eventsChannel = FlutterEventChannel(
      name: eventsChannelName,
      binaryMessenger: registrar.messenger
    )
    registrar.addMethodCallDelegate(instance, channel: playerChannel)
    eventsChannel.setStreamHandler(instance)
    registrar.register(ErikaVideoViewFactory(plugin: instance), withId: videoViewType)
  }

  public func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    do {
      switch call.method {
      case "create":
        result(try createPlayer(arguments: call.arguments))
      case "dispose":
        let args = try dictionaryArgs(call.arguments)
        let playerId = try requiredInt64(args["playerId"], name: "playerId")
        windowOverlayPlayerIds.remove(playerId)
        players.removeValue(forKey: playerId)
        stopPollTimerIfIdle()
        systemMediaNavigation.removeValue(forKey: playerId)
        if activePlayerId == playerId {
          activePlayerId = nil
          clearNowPlayingInfo()
          refreshRemoteCommands()
        }
        result(nil)
      case "open":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        let headers = (args["httpHeaders"] as? [String: String]) ?? [:]
        let readAhead = try optionalReadAheadBytes(args["httpReadAheadBytes"])
        let backBuffer = try optionalBackBufferBytes(args["httpBackBufferBytes"])
        if let metadata = args["metadata"] as? [String: Any] {
          try applyMediaMetadata(metadata, to: host)
        } else {
          host.clearMediaMetadata()
        }
        try host.open(
          uri: uri,
          httpHeaders: headers,
          httpReadAheadBytes: readAhead,
          httpBackBufferBytes: backBuffer
        )
        result(nil)
      case "play":
        let host = try playerHost(from: try dictionaryArgs(call.arguments))
        try host.play()
        activePlayerId = host.id
        refreshRemoteCommands()
        publishNowPlayingInfo(for: host)
        result(nil)
      case "pause":
        try playerHost(from: try dictionaryArgs(call.arguments)).pause()
        result(nil)
      case "stop":
        try playerHost(from: try dictionaryArgs(call.arguments)).stop()
        result(nil)
      case "close":
        try playerHost(from: try dictionaryArgs(call.arguments)).close()
        result(nil)
      case "createTexture":
        let args = try dictionaryArgs(call.arguments)
        let width = UInt32(clamping: max(1, int64Value(args["width"]) ?? 1))
        let height = UInt32(clamping: max(1, int64Value(args["height"]) ?? 1))
        let scale = max(0.1, doubleValue(args["scale"]) ?? 1.0)
        guard width <= 16384, height <= 16384,
              let texture = ErikaFlutterTextureSurface(
                registry: textureRegistry,
                width: width,
                height: height,
                scale: scale
              ) else {
          throw ErikaPluginError.invalidArguments("Unable to create Flutter texture surface.")
        }
        let textureId = texture.register()
        guard textureId != 0 else {
          throw ErikaPluginError.invalidArguments("Flutter rejected the texture surface.")
        }
        textures[textureId] = texture
        result(textureId)
      case "resizeTexture":
        let args = try dictionaryArgs(call.arguments)
        let textureId = try requiredInt64(args["textureId"], name: "textureId")
        guard let texture = textures[textureId] else {
          throw ErikaPluginError.viewNotFound(textureId)
        }
        let width = UInt32(clamping: max(1, int64Value(args["width"]) ?? 1))
        let height = UInt32(clamping: max(1, int64Value(args["height"]) ?? 1))
        let scale = max(0.1, doubleValue(args["scale"]) ?? 1.0)
        guard width <= 16384, height <= 16384,
              texture.resize(width: width, height: height, scale: scale) else {
          throw ErikaPluginError.invalidArguments("Unable to resize Flutter texture surface.")
        }
        texture.attachedPlayer?.resizeFromAttachedTexture()
        result(nil)
      case "releaseTexture":
        let args = try dictionaryArgs(call.arguments)
        let textureId = try requiredInt64(args["textureId"], name: "textureId")
        if let texture = textures.removeValue(forKey: textureId) {
          texture.attachedPlayer?.detach(viewId: textureId)
          textureRegistry.unregisterTexture(textureId)
        }
        result(nil)
      case "seek":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let positionMicros = try requiredUInt64(args["positionMicros"], name: "positionMicros")
        try host.seek(positionMicros: positionMicros)
        result(nil)
      case "setPlaybackRate":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let rate = doubleValue(args["rate"]) else {
          throw ErikaPluginError.invalidArguments("rate is required.")
        }
        try host.setPlaybackRate(rate)
        result(nil)
      case "setMediaMetadata":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let metadata = args["metadata"] as? [String: Any] else {
          throw ErikaPluginError.invalidArguments("metadata is required.")
        }
        try applyMediaMetadata(metadata, to: host)
        result(nil)
      case "setSystemMediaNavigation":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        systemMediaNavigation[host.id] = (
          previousEnabled: boolValue(args["previousEnabled"]) ?? false,
          nextEnabled: boolValue(args["nextEnabled"]) ?? false
        )
        if activePlayerId == host.id {
          refreshRemoteCommands()
          publishNowPlayingInfo(for: host)
        }
        result(nil)
      case "setVolume":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let volume = doubleValue(args["volume"]) else {
          throw ErikaPluginError.invalidArguments("volume is required.")
        }
        try host.setVolume(volume)
        result(nil)
      case "setUpscaler":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let mode = int32Value(args["mode"]) else {
          throw ErikaPluginError.invalidArguments("mode is required.")
        }
        try host.setUpscaler(mode: mode)
        result(nil)
      case "setSubtitleScale":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let scale = doubleValue(args["scale"]) else {
          throw ErikaPluginError.invalidArguments("scale is required.")
        }
        try host.setSubtitleScale(scale)
        result(nil)
      case "registerSubtitleMemoryFont":
        let args = try dictionaryArgs(call.arguments)
        guard let data = args["data"] as? FlutterStandardTypedData else { throw ErikaPluginError.invalidArguments("data is required.") }
        result(Int64(clamping: try playerHost(from: args).registerSubtitleMemoryFont(data.data)))
      case "selectSubtitleMemoryFonts":
        let args = try dictionaryArgs(call.arguments)
        try playerHost(from: args).selectSubtitleMemoryFonts((args["fontIds"] as? [NSNumber] ?? []).map { $0.uint64Value })
        result(nil)
      case "clearSubtitleMemoryFonts":
        let args = try dictionaryArgs(call.arguments)
        try playerHost(from: args).clearSubtitleMemoryFonts()
        result(nil)
      case "getSubtitleMemoryFontStatus":
        let args = try dictionaryArgs(call.arguments)
        result(try playerHost(from: args).subtitleMemoryFontStatus())
      case "setSubtitleStyle":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        if args.keys.contains("fontFamily") || args.keys.contains("fontFilePath") {
          try host.setSubtitleFont(
            family: args["fontFamily"] as? String,
            filePath: args["fontFilePath"] as? String
          )
        }
        if args.keys.contains("primaryColorRgba") || args.keys.contains("outlineColorRgba")
          || args.keys.contains("fontSize") || args.keys.contains("outlineWidth")
          || args.keys.contains("bold") || args.keys.contains("italic")
          || args.keys.contains("underline") || args.keys.contains("strikeOut")
          || args.keys.contains("spacing") || args.keys.contains("scaleXPercent")
          || args.keys.contains("scaleYPercent") || args.keys.contains("borderStyle")
          || args.keys.contains("shadowDepth") || args.keys.contains("blur")
          || args.keys.contains("alignment") || args.keys.contains("marginLeft")
          || args.keys.contains("marginRight") || args.keys.contains("marginVertical")
          || args.keys.contains("overrideMask")
        {
          let primary = int64Value(args["primaryColorRgba"]) ?? 0xFFFF_FFFF
          let outline = int64Value(args["outlineColorRgba"]) ?? 0x0000_007F
          try host.setSubtitleStyle(
            fontFamily: args["fontFamily"] as? String,
            fontFilePath: args["fontFilePath"] as? String,
            primaryRgba: UInt32(truncatingIfNeeded: primary),
            outlineRgba: UInt32(truncatingIfNeeded: outline),
            fontSize: doubleValue(args["fontSize"]) ?? 48.0,
            outlineWidth: doubleValue(args["outlineWidth"]) ?? 2.0,
            bold: boolValue(args["bold"]) ?? false,
            italic: boolValue(args["italic"]) ?? false,
            underline: boolValue(args["underline"]) ?? false,
            strikeOut: boolValue(args["strikeOut"]) ?? false,
            spacing: doubleValue(args["spacing"]) ?? 0.0,
            scaleXPercent: doubleValue(args["scaleXPercent"]) ?? 100.0,
            scaleYPercent: doubleValue(args["scaleYPercent"]) ?? 100.0,
            borderStyle: int32Value(args["borderStyle"]) ?? 1,
            shadowDepth: doubleValue(args["shadowDepth"]) ?? 0.0,
            blur: doubleValue(args["blur"]) ?? 0.0,
            alignment: int32Value(args["alignment"]) ?? 2,
            marginLeft: int32Value(args["marginLeft"]) ?? 48,
            marginRight: int32Value(args["marginRight"]) ?? 48,
            marginVertical: int32Value(args["marginVertical"]) ?? 54,
            overrideMask: UInt32(truncatingIfNeeded: int64Value(args["overrideMask"]) ?? 0)
          )
        }
        result(nil)
      case "getUpscalerStatus":
        let args = try dictionaryArgs(call.arguments)
        result(try playerHost(from: args).upscalerStatus())
      case "getOutputStatus":
        let args = try dictionaryArgs(call.arguments)
        result(try playerHost(from: args).outputStatus())
      case "getResourceStatus":
        let args = try dictionaryArgs(call.arguments)
        result(try playerHost(from: args).resourceStatus())
      case "getDiagnosticsSnapshot":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        respondOffMain(queue: host.jsonQueue, keeping: host, result: result) {
          try host.diagnosticsSnapshotJson()
        }
      case "invokeJson":
        let args = try dictionaryArgs(call.arguments)
        guard let method = args["method"] as? String, !method.isEmpty else {
          throw ErikaPluginError.invalidArguments("method is required.")
        }
        let rawArguments = args["arguments"]
        let arguments: [String: Any]
        if rawArguments == nil || rawArguments is NSNull {
          arguments = [:]
        } else if let dictionary = rawArguments as? [String: Any] {
          arguments = dictionary
        } else {
          throw ErikaPluginError.invalidArguments("arguments must be a map.")
        }
        guard JSONSerialization.isValidJSONObject(arguments) else {
          throw ErikaPluginError.invalidArguments("arguments must be JSON-compatible.")
        }
        let data = try JSONSerialization.data(withJSONObject: arguments)
        let argumentsJson = String(decoding: data, as: UTF8.self)
        let host = try playerHost(from: args)
        respondOffMain(queue: host.jsonQueue, keeping: host, result: result) {
          try host.invokeJson(method: method, argumentsJson: argumentsJson)
        }
      case "getLogs":
        // The log ring is process-wide; playerId is accepted but not needed.
        let args = try dictionaryArgs(call.arguments)
        let limit = try diagnosticsCount(args["limit"], name: "limit", defaultValue: 200)
        let sinceSeq = try diagnosticsCount(args["sinceSeq"], name: "sinceSeq", defaultValue: 0)
        let minLevel = try diagnosticsLogLevel(args["minLevel"])
        respondOffMain(queue: logQueue, keeping: nil, result: result) {
          guard let library = ErikaNativeLibrary.shared else {
            throw ErikaPluginError.libraryNotFound([])
          }
          return try library.readLogsJson(
            limit: UInt32(min(limit, UInt64(UInt32.max))),
            minLevel: minLevel,
            sinceSeq: sinceSeq
          )
        }
      case "getPresenterStats":
        let args = try dictionaryArgs(call.arguments)
        result(try playerHost(from: args).presenterStats())
      case "setDebugHudEnabled":
        let args = try dictionaryArgs(call.arguments)
        try playerHost(from: args).setDebugHudEnabled(boolValue(args["enabled"]) ?? false)
        result(nil)
      case "addExternalSubtitle":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        result(try host.addExternalSubtitle(uri: uri))
      case "removeSubtitleTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let trackId = try requiredInt64(args["trackId"], name: "trackId")
        try host.removeSubtitleTrack(trackId: trackId)
        result(nil)
      case "loadDanmakuFile":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        try host.loadDanmakuFile(uri: uri)
        result(nil)
      case "loadDanmakuJson":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let json = args["json"] as? String, !json.isEmpty else {
          throw ErikaPluginError.invalidArguments("json is required.")
        }
        try host.loadDanmakuJson(json)
        result(nil)
      case "addDanmakuTrackFile":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        let offsetMicros = int64Value(args["offsetMicros"]) ?? 0
        result(Int64(clamping: try host.addDanmakuTrackFile(
          uri: uri,
          name: args["name"] as? String,
          offsetMicros: offsetMicros
        )))
      case "addDanmakuTrackJson":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        guard let json = args["json"] as? String, !json.isEmpty else {
          throw ErikaPluginError.invalidArguments("json is required.")
        }
        let offsetMicros = int64Value(args["offsetMicros"]) ?? 0
        result(Int64(clamping: try host.addDanmakuTrackJson(
          json,
          name: args["name"] as? String,
          offsetMicros: offsetMicros
        )))
      case "removeDanmakuTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.removeDanmakuTrack(trackId: try requiredUInt64(args["trackId"], name: "trackId"))
        result(nil)
      case "setDanmakuTrackEnabled":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.setDanmakuTrackEnabled(
          trackId: try requiredUInt64(args["trackId"], name: "trackId"),
          enabled: boolValue(args["enabled"]) ?? true
        )
        result(nil)
      case "setDanmakuTrackOffset":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.setDanmakuTrackOffset(
          trackId: try requiredUInt64(args["trackId"], name: "trackId"),
          offsetMicros: int64Value(args["offsetMicros"]) ?? 0
        )
        result(nil)
      case "setDanmakuGlobalOffset":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.setDanmakuGlobalOffset(offsetMicros: int64Value(args["offsetMicros"]) ?? 0)
        result(nil)
      case "danmakuTracks":
        let host = try playerHost(from: try dictionaryArgs(call.arguments))
        result(try host.danmakuTracks())
      case "clearDanmaku":
        let host = try playerHost(from: try dictionaryArgs(call.arguments))
        try host.clearDanmaku()
        result(nil)
      case "setDanmakuEnabled":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.setDanmakuEnabled(boolValue(args["enabled"]) ?? true)
        result(nil)
      case "setDanmakuConfig":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.setDanmakuConfig(
          danmakuConfig(from: args, base: host.danmakuConfigSnapshot())
        )
        if args.keys.contains("customFontFamily") || args.keys.contains("customFontFilePath") {
          try host.setDanmakuFont(
            family: args["customFontFamily"] as? String,
            filePath: args["customFontFilePath"] as? String
          )
        }
        if let blockWordsJson = args["blockWordsJson"] as? String {
          try host.setDanmakuBlockWordsJson(blockWordsJson)
        }
        result(nil)
      case "selectAudioTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.selectAudioTrack(trackId: optionalTrackId(args["trackId"]))
        result(nil)
      case "selectSubtitleTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        try host.selectSubtitleTrack(trackId: optionalTrackId(args["trackId"]))
        result(nil)
      case "tracks":
        let host = try playerHost(from: try dictionaryArgs(call.arguments))
        result(try host.tracks())
      case "screenshot":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let view = try optionalVideoView(from: args, host: host)
        let width = int64Value(args["width"]).map(Int.init)
        let height = int64Value(args["height"]).map(Int.init)
        if let data = host.screenshot(view: view, width: width, height: height) {
          result(FlutterStandardTypedData(bytes: data))
        } else {
          result(nil)
        }
      case "exportGif":
        let args = try dictionaryArgs(call.arguments)
        guard let inputUri = args["inputUri"] as? String, !inputUri.isEmpty else {
          throw ErikaPluginError.invalidArguments("inputUri is required.")
        }
        guard let outputPath = args["outputPath"] as? String, !outputPath.isEmpty else {
          throw ErikaPluginError.invalidArguments("outputPath is required.")
        }
        let startMillis = try requiredUInt64(args["startMillis"], name: "startMillis")
        let endMillis = try requiredUInt64(args["endMillis"], name: "endMillis")
        let framesPerSecondValue = try requiredUInt64(args["framesPerSecond"], name: "framesPerSecond")
        let outputWidthValue = try requiredUInt64(args["outputWidth"], name: "outputWidth")
        let outputHeightValue = try requiredUInt64(args["outputHeight"], name: "outputHeight")
        guard let framesPerSecond = UInt32(exactly: framesPerSecondValue),
              let outputWidth = UInt32(exactly: outputWidthValue),
              let outputHeight = UInt32(exactly: outputHeightValue),
              let quality = Int32(exactly: int64Value(args["quality"]) ?? 0),
              let loopCount = Int32(exactly: int64Value(args["loopCount"]) ?? 0) else {
          throw ErikaPluginError.invalidArguments("GIF export numeric arguments are out of range.")
        }
        let overwrite = (args["overwrite"] as? Bool) ?? true
        let httpHeaders = (args["httpHeaders"] as? [String: String]) ?? [:]
        let httpReadAheadBytes = try optionalReadAheadBytes(args["httpReadAheadBytes"])
        DispatchQueue.global(qos: .userInitiated).async {
          do {
            guard let library = ErikaNativeLibrary.shared else {
              throw ErikaPluginError.libraryNotFound([])
            }
            let exported = try library.exportGif(
              inputUri: inputUri,
              outputPath: outputPath,
              startMillis: startMillis,
              endMillis: endMillis,
              framesPerSecond: framesPerSecond,
              outputWidth: outputWidth,
              outputHeight: outputHeight,
              quality: quality,
              loopCount: loopCount,
              overwrite: overwrite,
              httpHeaders: httpHeaders,
              httpReadAheadBytes: httpReadAheadBytes
            )
            DispatchQueue.main.async {
              result([
                "outputPath": outputPath,
                "width": Int(exported.width),
                "height": Int(exported.height),
                "frameCount": Int64(exported.frameCount),
                "fileSize": Int64(exported.fileSize),
              ])
            }
          } catch {
            DispatchQueue.main.async { result(self.flutterError(error)) }
          }
        }
      case "exportFrame":
        let args = try dictionaryArgs(call.arguments)
        guard let inputUri = args["uri"] as? String, !inputUri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        let positionMillis = try requiredUInt64(args["positionMillis"], name: "positionMillis")
        guard let maxWidth = UInt32(exactly: int64Value(args["width"]) ?? 0),
              let maxHeight = UInt32(exactly: int64Value(args["height"]) ?? 0),
              let format = Int32(exactly: int64Value(args["format"]) ?? 1) else {
          throw ErikaPluginError.invalidArguments("Frame export numeric arguments are out of range.")
        }
        let httpHeaders = (args["httpHeaders"] as? [String: String]) ?? [:]
        let httpReadAheadBytes = try optionalReadAheadBytes(args["httpReadAheadBytes"])
        // Decoding (a 4K keyframe interval) takes up to a second or so: never
        // on the main thread.
        DispatchQueue.global(qos: .userInitiated).async {
          do {
            guard let library = ErikaNativeLibrary.shared else {
              throw ErikaPluginError.libraryNotFound([])
            }
            let exported = try library.exportFrame(
              inputUri: inputUri,
              positionMillis: positionMillis,
              maxWidth: maxWidth,
              maxHeight: maxHeight,
              format: format,
              httpHeaders: httpHeaders,
              httpReadAheadBytes: httpReadAheadBytes
            )
            DispatchQueue.main.async { result(exported) }
          } catch {
            DispatchQueue.main.async { result(self.flutterError(error)) }
          }
        }
      case "openFrameExportSession":
        let args = try dictionaryArgs(call.arguments)
        guard let inputUri = args["uri"] as? String, !inputUri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        let httpHeaders = (args["httpHeaders"] as? [String: String]) ?? [:]
        let httpReadAheadBytes = try optionalReadAheadBytes(args["httpReadAheadBytes"])
        let sessionId = nextFrameExportSessionId
        nextFrameExportSessionId += 1
        let box = ErikaFrameExportSessionBox(id: sessionId)
        // Opening reads the container header, which for a network source takes
        // a round trip: off the main thread.
        box.queue.async {
          do {
            guard let library = ErikaNativeLibrary.shared else {
              throw ErikaPluginError.libraryNotFound([])
            }
            try box.open(
              library: library,
              inputUri: inputUri,
              httpHeaders: httpHeaders,
              httpReadAheadBytes: httpReadAheadBytes
            )
            DispatchQueue.main.async {
              self.frameExportSessions[sessionId] = box
              result(sessionId)
            }
          } catch {
            DispatchQueue.main.async { result(self.flutterError(error)) }
          }
        }
      case "exportSessionFrame":
        let args = try dictionaryArgs(call.arguments)
        let sessionId = try requiredInt64(args["sessionId"], name: "sessionId")
        guard let box = frameExportSessions[sessionId] else {
          throw ErikaPluginError.invalidArguments("Frame export session \(sessionId) is not open.")
        }
        let positionMillis = try requiredUInt64(args["positionMillis"], name: "positionMillis")
        guard let maxWidth = UInt32(exactly: int64Value(args["width"]) ?? 0),
              let maxHeight = UInt32(exactly: int64Value(args["height"]) ?? 0),
              let format = Int32(exactly: int64Value(args["format"]) ?? 1) else {
          throw ErikaPluginError.invalidArguments("Frame export numeric arguments are out of range.")
        }
        respondOffMain(queue: box.queue, keeping: box, result: result) {
          try box.frame(
            positionMillis: positionMillis,
            maxWidth: maxWidth,
            maxHeight: maxHeight,
            format: format
          )
        }
      case "closeFrameExportSession":
        let args = try dictionaryArgs(call.arguments)
        let sessionId = try requiredInt64(args["sessionId"], name: "sessionId")
        guard let box = frameExportSessions.removeValue(forKey: sessionId) else {
          result(nil)
          return
        }
        respondOffMain(queue: box.queue, keeping: box, result: result) {
          box.close()
          return Optional<Int>.none
        }
      case "attachView":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let viewId = try requiredInt64(args["viewId"], name: "viewId")
        if let texture = textures[viewId] {
          try host.attach(texture: texture)
        } else if let view = views[viewId]?.view {
          try host.attach(view: view)
        } else {
          throw ErikaPluginError.viewNotFound(viewId)
        }
        result(nil)
      case "detachView":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let viewId = try requiredInt64(args["viewId"], name: "viewId")
        host.detach(viewId: viewId)
        if viewId == erikaWindowHostedVideoSurfaceId {
          windowOverlayPlayerIds.remove(host.id)
        }
        result(nil)
      case "attachOverlay":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        updateRequestedFlutterView(from: args)
        let installation = try ensureWindowOverlayInstalled()
        erikaWindowOverlayTrace(
          "attach request player=\(host.id) flutterView=\(requestedFlutterViewIdentifier ?? -1) " +
          "secondary=\(requestedSecondaryWindow) targetWindow=\(installation.hostWindow.windowNumber)"
        )
        let overlay = installation.overlay
        try host.attach(view: overlay)
        windowOverlayPlayerIds.insert(host.id)
        result(erikaWindowHostedVideoSurfaceId)
      case "detachOverlay":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let generation = int64Value(args["generation"])
        let overlay = resolveWindowOverlay()
        // A disposing widget can fire detachOverlay after a newer widget has
        // already re-attached the shared overlay surface. Skip the teardown so
        // the stale detach cannot stop the live surface's display link and
        // leave a frozen, non-rendering overlay on screen.
        if let generation,
           let activeGeneration = overlay?.activeGeneration,
           generation != activeGeneration {
          result(nil)
          return
        }
        host.detach(viewId: erikaWindowHostedVideoSurfaceId)
        windowOverlayPlayerIds.remove(host.id)
        overlay?.updateOverlayFrame(nil, visible: false, debugLabel: nil, generation: generation)
        result(nil)
      case "setOverlayFrame":
        let args = try dictionaryArgs(call.arguments)
        let visible = boolValue(args["visible"]) ?? true
        let generation = int64Value(args["generation"])
        if !visible,
           let generation,
           let activeGeneration = resolveWindowOverlay()?.activeGeneration,
           generation != activeGeneration {
          result(nil)
          return
        }
        updateRequestedFlutterView(from: args)
        let installation = try ensureWindowOverlayInstalled()
        let overlay = installation.overlay
        let frame = convertedOverlayRect(
          from: args,
          sourceView: installation.flutterHostView,
          targetView: overlay
        )
        overlay.updateOverlayFrame(
          frame,
          visible: visible,
          debugLabel: args["debugLabel"] as? String,
          generation: generation
        )
        if installation.movedBetweenWindows {
          // A wgpu/Metal surface retains the CAMetalLayer pointer, but moving
          // that layer into another NSWindow invalidates its drawable chain on
          // some macOS/Flutter combinations. Re-attach after the new frame and
          // backing scale are known so rendering resumes in the destination.
          for playerId in windowOverlayPlayerIds {
            if let attachedHost = players[playerId] {
              try attachedHost.attach(view: overlay)
            }
          }
        }
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    } catch {
      result(flutterError(error))
    }
  }

  public func onListen(withArguments arguments: Any?, eventSink events: @escaping FlutterEventSink) -> FlutterError? {
    Self.sharedEventSink = events
    startPollTimerIfNeeded()
    return nil
  }

  public func onCancel(withArguments arguments: Any?) -> FlutterError? {
    Self.sharedEventSink = nil
    return nil
  }

  fileprivate func registerView(_ view: NSView & ErikaMetalSurfaceView, viewId: Int64) {
    views[viewId] = WeakErikaVideoPlatformViewBox(view: view)
  }

  fileprivate func unregisterView(viewId: Int64) {
    views.removeValue(forKey: viewId)
    for host in players.values {
      host.detach(viewId: viewId)
    }
  }

  fileprivate func resizePlayerAttachedToView(viewId: Int64) {
    for host in players.values {
      if let attachedPlayerId = views[viewId]?.view?.attachedPlayerId,
         attachedPlayerId == host.id {
        host.resizeFromAttachedView()
      }
    }
  }

  fileprivate func detachOverlayView(_ view: ErikaWindowOverlayView) {
    for host in players.values {
      host.detach(viewId: view.platformViewId)
    }
    if views[view.platformViewId]?.view === view {
      views.removeValue(forKey: view.platformViewId)
    }
    if view.window?.erikaWindowOverlayView === view {
      view.window?.erikaWindowOverlayView = nil
    }
  }

  private struct WindowOverlayInstallation {
    let overlay: ErikaWindowOverlayView
    let flutterHostView: NSView
    let hostWindow: NSWindow
    let movedBetweenWindows: Bool
  }

  private func ensureWindowOverlayInstalled() throws -> WindowOverlayInstallation {
    guard let activeFlutterHostView else {
      erikaWindowOverlayTrace(
        "target unavailable flutterView=\(requestedFlutterViewIdentifier ?? -1) " +
        "secondary=\(requestedSecondaryWindow); refusing main-window fallback"
      )
      throw ErikaPluginError.overlayNotAvailable
    }
    guard let hostWindow = activeFlutterHostView.window else {
      throw ErikaPluginError.overlayNotAvailable
    }
    guard let hostSuperview = activeFlutterHostView.superview else {
      throw ErikaPluginError.overlayNotAvailable
    }

    let existingOverlay = resolveWindowOverlay()
    // `setOverlayFrame` runs on every geometry update. Once the overlay is
    // already mounted in the right place there is nothing to install, and
    // re-adding it would restart the whole attach path (viewDidMoveToWindow →
    // updateDrawableSize → resize → display driver) on every resize frame.
    if let existingOverlay,
       existingOverlay.superview === hostSuperview,
       existingOverlay.window === hostWindow {
      existingOverlay.plugin = self
      return WindowOverlayInstallation(
        overlay: existingOverlay,
        flutterHostView: activeFlutterHostView,
        hostWindow: hostWindow,
        movedBetweenWindows: false
      )
    }

    prepareFlutterHostViewForWindowOverlay(activeFlutterHostView)

    let overlay = existingOverlay ??
      hostWindow.erikaWindowOverlayView ??
      ErikaWindowOverlayView(plugin: self)
    let previousWindow = overlay.window
    overlay.plugin = self

    if overlay.superview !== hostSuperview {
      overlay.removeFromSuperview()
      overlay.frame = .zero
      overlay.translatesAutoresizingMaskIntoConstraints = true
    }
    hostSuperview.addSubview(
      overlay,
      positioned: shouldPlaceWindowOverlayAboveFlutter() ? .above : .below,
      relativeTo: activeFlutterHostView
    )

    if previousWindow !== hostWindow {
      previousWindow?.erikaWindowOverlayView = nil
      erikaWindowOverlayTrace(
        "move surface flutterView=\(requestedFlutterViewIdentifier ?? -1) " +
        "secondary=\(requestedSecondaryWindow) " +
        "window=\(previousWindow?.windowNumber ?? -1)->\(hostWindow.windowNumber)"
      )
    }
    hostWindow.erikaWindowOverlayView = overlay
    registerView(overlay, viewId: overlay.platformViewId)
    if existingOverlay == nil {
      for playerId in windowOverlayPlayerIds {
        if let host = players[playerId] {
          try host.attach(view: overlay)
        }
      }
    }
    return WindowOverlayInstallation(
      overlay: overlay,
      flutterHostView: activeFlutterHostView,
      hostWindow: hostWindow,
      movedBetweenWindows: existingOverlay != nil && previousWindow !== hostWindow
    )
  }

  private func updateRequestedFlutterView(from args: [String: Any]) {
    if let flutterViewIdentifier = int64Value(args["flutterViewId"]) {
      requestedFlutterViewIdentifier = flutterViewIdentifier
    }
    if let secondaryWindow = boolValue(args["secondaryWindow"]) {
      requestedSecondaryWindow = secondaryWindow
    }
  }

  /// The player widget can move between FlutterViews without rebuilding its
  /// State. Resolve the native host from the view id sent by Dart so the same
  /// Metal overlay follows the widget into (and back out of) a detached window.
  private var activeFlutterHostView: NSView? {
    if let requestedFlutterViewIdentifier {
      return NSApp.windows
        .compactMap({ $0.contentViewController as? FlutterViewController })
        .first(where: {
          Int64($0.viewIdentifier) == requestedFlutterViewIdentifier
        })?
        .view
    }
    if let keyController = NSApp.keyWindow?.contentViewController
        as? FlutterViewController {
      return keyController.view
    }
    if let mainController = NSApp.mainWindow?.contentViewController
        as? FlutterViewController {
      return mainController.view
    }
    return flutterHostView ?? flutterHostViewController?.view
  }

  private func resolveWindowOverlay() -> ErikaWindowOverlayView? {
    if let hostWindow = activeFlutterHostView?.window,
       let overlay = hostWindow.erikaWindowOverlayView {
      return overlay
    }
    if let hostWindow = flutterHostView?.window,
       let overlay = hostWindow.erikaWindowOverlayView {
      return overlay
    }
    if let hostWindow = flutterHostViewController?.view.window,
       let overlay = hostWindow.erikaWindowOverlayView {
      return overlay
    }
    if let overlay = NSApp.keyWindow?.erikaWindowOverlayView {
      return overlay
    }
    if let overlay = NSApp.mainWindow?.erikaWindowOverlayView {
      return overlay
    }
    return NSApp.windows.compactMap(\.erikaWindowOverlayView).first
  }

  private func shouldPlaceWindowOverlayAboveFlutter() -> Bool {
    let environment = ProcessInfo.processInfo.environment
    if environment["ERIKA_WINDOW_OVERLAY_BELOW"] == "1" {
      return false
    }
    return environment["ERIKA_WINDOW_OVERLAY_ABOVE"] == "1"
  }

  private func prepareFlutterHostViewForWindowOverlay(_ view: NSView) {
    if shouldPlaceWindowOverlayAboveFlutter() {
      return
    }
    view.wantsLayer = true
    view.layer?.isOpaque = false
    view.layer?.backgroundColor = NSColor.clear.cgColor
    if let targetController = view.window?.contentViewController
        as? FlutterViewController {
      targetController.backgroundColor = .clear
    }
    view.window?.isOpaque = false
    view.window?.backgroundColor = .clear
  }

  private func convertedOverlayRect(
    from args: [String: Any],
    sourceView: NSView,
    targetView: NSView
  ) -> CGRect? {
    guard let x = doubleValue(args["x"]),
          let y = doubleValue(args["y"]),
          let width = doubleValue(args["width"]),
          let height = doubleValue(args["height"]) else {
      return nil
    }
    guard width > 0, height > 0 else {
      return nil
    }
    guard let targetSuperview = targetView.superview else {
      return CGRect(x: x, y: y, width: width, height: height)
    }
    let sourceY = sourceView.isFlipped
      ? y
      : sourceView.bounds.height - y - height
    let rect = CGRect(x: x, y: sourceY, width: width, height: height)
    return sourceView.convert(rect, to: targetSuperview)
  }

  private func createPlayer(arguments: Any?) throws -> Int64 {
    guard let library = ErikaNativeLibrary.shared else {
      throw ErikaPluginError.libraryNotFound([
        ProcessInfo.processInfo.environment["ERIKA_CAPI_DYLIB"] ?? "",
        ErikaNativeLibrary.sourceTreeDebugLibraryPath() ?? "",
        "liberika_capi.dylib",
      ].filter { !$0.isEmpty })
    }
    let id = nextPlayerId
    nextPlayerId += 1
    let host = try ErikaPlayerHost(
      id: id,
      library: library,
      config: presenterConfigForNewPlayer(arguments: arguments)
    )
    host.onNowPlayingChanged = { [weak self] changedHost in
      guard self?.activePlayerId == changedHost.id else { return }
      self?.handleNowPlayingChanged(for: changedHost)
    }
    players[id] = host
    systemMediaNavigation[id] = (previousEnabled: false, nextEnabled: false)
    startPollTimerIfNeeded()
    return id
  }

  private func configureSystemPlayback() {
    let commands = MPRemoteCommandCenter.shared()
    addRemoteTarget(commands.playCommand) { [weak self] _ in
      self?.performRemoteCommand { try $0.play() } ?? .commandFailed
    }
    addRemoteTarget(commands.pauseCommand) { [weak self] _ in
      self?.performRemoteCommand { try $0.pause() } ?? .commandFailed
    }
    addRemoteTarget(commands.togglePlayPauseCommand) { [weak self] _ in
      self?.performRemoteCommand { host in
        if host.isPlaying {
          try host.pause()
        } else {
          try host.play()
        }
      } ?? .commandFailed
    }
    addRemoteTarget(commands.stopCommand) { [weak self] _ in
      self?.performRemoteCommand { try $0.stop() } ?? .commandFailed
    }
    addRemoteTarget(commands.changePlaybackPositionCommand) { [weak self] event in
      guard let positionEvent = event as? MPChangePlaybackPositionCommandEvent else {
        return .commandFailed
      }
      return self?.performRemoteSeek(positionEvent.positionTime) ?? .commandFailed
    }
    addRemoteTarget(commands.previousTrackCommand) { [weak self] _ in
      self?.emitSystemMediaNavigation("previous") ?? .commandFailed
    }
    addRemoteTarget(commands.nextTrackCommand) { [weak self] _ in
      self?.emitSystemMediaNavigation("next") ?? .commandFailed
    }
    refreshRemoteCommands()
  }

  private func addRemoteTarget(
    _ command: MPRemoteCommand,
    handler: @escaping (MPRemoteCommandEvent) -> MPRemoteCommandHandlerStatus
  ) {
    command.isEnabled = false
    let target = command.addTarget(handler: handler)
    remoteCommandTargets.append((command, target))
  }

  private func applyMediaMetadata(_ metadata: [String: Any], to host: ErikaPlayerHost) throws {
    guard let title = metadata["title"] as? String, !title.isEmpty else {
      throw ErikaPluginError.invalidArguments("metadata.title is required.")
    }
    try host.setMediaMetadata(
      title: title,
      artist: metadata["artist"] as? String,
      album: metadata["album"] as? String,
      artworkData: (metadata["artwork"] as? FlutterStandardTypedData)?.data
    )
  }

  private func handleNowPlayingChanged(for host: ErikaPlayerHost) {
    if host.playbackState == 6 {
      activePlayerId = nil
      clearNowPlayingInfo()
      refreshRemoteCommands()
      return
    }
    publishNowPlayingInfo(for: host)
  }

  private func publishNowPlayingInfo(
    for host: ErikaPlayerHost,
    playbackState: MPNowPlayingPlaybackState? = nil
  ) {
    let resolvedPlaybackState = playbackState ??
      (host.isPlaying ? .playing : host.isStopped ? .stopped : .paused)
    var info: [String: Any] = [
      MPMediaItemPropertyTitle: host.nowPlayingTitle,
      MPNowPlayingInfoPropertyElapsedPlaybackTime: host.nowPlayingPositionSeconds,
      MPNowPlayingInfoPropertyPlaybackRate: resolvedPlaybackState == .playing ? host.playbackRate : 0,
      MPNowPlayingInfoPropertyDefaultPlaybackRate: host.playbackRate,
      MPNowPlayingInfoPropertyMediaType: MPNowPlayingInfoMediaType.video.rawValue,
    ]
    if let artist = host.nowPlayingArtist { info[MPMediaItemPropertyArtist] = artist }
    if let album = host.nowPlayingAlbum { info[MPMediaItemPropertyAlbumTitle] = album }
    if let artwork = host.nowPlayingArtwork { info[MPMediaItemPropertyArtwork] = artwork }
    if let duration = host.durationSeconds { info[MPMediaItemPropertyPlaybackDuration] = duration }
    let center = MPNowPlayingInfoCenter.default()
    center.nowPlayingInfo = info
    center.playbackState = resolvedPlaybackState
  }

  private func clearNowPlayingInfo() {
    let center = MPNowPlayingInfoCenter.default()
    center.nowPlayingInfo = nil
    center.playbackState = .stopped
  }

  private func activePlayer() -> ErikaPlayerHost? {
    activePlayerId.flatMap { players[$0] }
  }

  private func performRemoteCommand(
    _ command: @escaping (ErikaPlayerHost) throws -> Void
  ) -> MPRemoteCommandHandlerStatus {
    performOnMain {
      guard let host = self.activePlayer() else { return .noSuchContent }
      do {
        try command(host)
        return .success
      } catch {
        return .commandFailed
      }
    }
  }

  private func performRemoteSeek(_ position: TimeInterval) -> MPRemoteCommandHandlerStatus {
    guard position.isFinite else { return .commandFailed }
    return performRemoteCommand { host in
      let duration = host.durationSeconds ?? position
      let boundedPosition = min(max(0, position), max(0, duration))
      try host.seek(positionMicros: UInt64(boundedPosition * 1_000_000))
    }
  }

  private func emitSystemMediaNavigation(_ navigation: String) -> MPRemoteCommandHandlerStatus {
    performOnMain {
      guard let playerId = self.activePlayerId,
            self.players[playerId] != nil,
            let capabilities = self.systemMediaNavigation[playerId] else { return .noSuchContent }
      let enabled = navigation == "previous"
        ? capabilities.previousEnabled
        : capabilities.nextEnabled
      guard enabled else { return .noSuchContent }
      Self.sharedEventSink?([
        "playerId": playerId,
        "kind": 13,
        "navigation": navigation,
      ])
      return .success
    }
  }

  private func performOnMain(
    _ work: @escaping () -> MPRemoteCommandHandlerStatus
  ) -> MPRemoteCommandHandlerStatus {
    if Thread.isMainThread {
      return work()
    }
    return DispatchQueue.main.sync(execute: work)
  }

  private func refreshRemoteCommands() {
    let commands = MPRemoteCommandCenter.shared()
    let enabled = activePlayer() != nil
    remoteCommandTargets.forEach { command, _ in
      command.isEnabled = enabled
    }
    let capabilities = activePlayerId.flatMap { systemMediaNavigation[$0] }
    commands.previousTrackCommand.isEnabled = enabled && capabilities?.previousEnabled == true
    commands.nextTrackCommand.isEnabled = enabled && capabilities?.nextEnabled == true
  }

  private func presenterConfigForNewPlayer(arguments: Any?) throws -> ErikaPresenterConfigC {
    let alphaMode = (arguments as? [String: Any])
      .flatMap { int32Value($0["videoAlphaMode"]) } ?? 0
    // An explicit disable must become SDR, not `auto(headroom: 1.0)`: under
    // the per-presenting-display Auto negotiation a headroom of 1.0 means "no
    // embedder cap, defer to the display", so an EDR display would promote
    // again and ignore ERIKA_DISABLE_EDR.
    let edrDisabled = boolEnvironmentFlag(
      "ERIKA_DISABLE_EDR",
      environment: ProcessInfo.processInfo.environment
    )
    if let args = arguments as? [String: Any],
       let explicitMode = int32Value(args["outputMode"]) {
      let headroom = floatValue(args["edrHeadroom"]) ?? 4.0
      var config: ErikaPresenterConfigC
      switch explicitMode {
      case 1:
        config = .appleEdr(headroom: headroom)
      case 2:
        config = ErikaPresenterConfigC(outputMode: 2, edrHeadroom: max(1.0, headroom))
      case 3:
        config = edrDisabled ? .sdr : .auto(headroom: headroom)
      default:
        config = .sdr
      }
      config.videoAlphaMode = alphaMode
      return config
    }

    guard let headroom = resolvedEdrHeadroom() else {
      NSLog("ErikaFlutterPlugin: ERIKA_DISABLE_EDR is set; using SDR output")
      var config = ErikaPresenterConfigC.sdr
      config.videoAlphaMode = alphaMode
      return config
    }
    NSLog("ErikaFlutterPlugin: using automatic Apple output, headroom \(headroom)x")
    let config = ErikaPresenterConfigC.auto(headroom: headroom)
    var alphaConfig = config
    alphaConfig.videoAlphaMode = alphaMode
    return alphaConfig
  }

  /// EDR headroom to request for a new player, or `nil` when
  /// `ERIKA_DISABLE_EDR` disables EDR and the player must use SDR.
  private func resolvedEdrHeadroom() -> Float? {
    let environment = ProcessInfo.processInfo.environment
    if boolEnvironmentFlag("ERIKA_DISABLE_EDR", environment: environment) {
      return nil
    }
    if let override = floatEnvironmentValue("ERIKA_EDR_HEADROOM", environment: environment),
       override > 1.0 {
      return override
    }

    let screenHeadroom = currentScreenEdrHeadroom()
    if screenHeadroom > 1.0 {
      return screenHeadroom
    }
    if boolEnvironmentFlag("ERIKA_ENABLE_EDR", environment: environment) {
      return 4.0
    }
    return 1.0
  }

  private func currentScreenEdrHeadroom() -> Float {
    let screen = flutterHostView?.window?.screen ??
      flutterHostViewController?.view.window?.screen ??
      NSApp.keyWindow?.screen ??
      NSApp.mainWindow?.screen ??
      NSScreen.main
    return screenPotentialEdrHeadroom(screen)
  }

  private func boolEnvironmentFlag(
    _ name: String,
    environment: [String: String]
  ) -> Bool {
    switch environment[name]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
    case "1", "true", "yes", "on":
      return true
    default:
      return false
    }
  }

  private func floatEnvironmentValue(
    _ name: String,
    environment: [String: String]
  ) -> Float? {
    guard let raw = environment[name]?.trimmingCharacters(in: .whitespacesAndNewlines),
          !raw.isEmpty,
          let value = Float(raw),
          value.isFinite else {
      return nil
    }
    return value
  }

  private func int32Value(_ value: Any?) -> Int32? {
    if let value = value as? Int32 {
      return value
    }
    if let value = value as? NSNumber {
      return value.int32Value
    }
    if let value = value as? String {
      return Int32(value)
    }
    return nil
  }

  private func floatValue(_ value: Any?) -> Float? {
    if let value = value as? Float, value.isFinite {
      return value
    }
    if let value = value as? Double, value.isFinite {
      return Float(value)
    }
    if let value = value as? NSNumber {
      let result = value.floatValue
      return result.isFinite ? result : nil
    }
    if let value = value as? String,
       let result = Float(value),
       result.isFinite {
      return result
    }
    return nil
  }

  private func playerHost(from args: [String: Any]) throws -> ErikaPlayerHost {
    let playerId = try requiredInt64(args["playerId"], name: "playerId")
    guard let host = players[playerId] else {
      throw ErikaPluginError.playerNotFound(playerId)
    }
    return host
  }

  private func optionalVideoView(
    from args: [String: Any],
    host: ErikaPlayerHost
  ) throws -> (NSView & ErikaMetalSurfaceView)? {
    guard let viewId = int64Value(args["viewId"]) else {
      return nil
    }
    guard let view = views[viewId]?.view,
          view.attachedPlayerId == host.id else {
      throw ErikaPluginError.viewNotFound(viewId)
    }
    return view
  }

  private func optionalTrackId(_ value: Any?) throws -> Int64? {
    if value == nil || value is NSNull {
      return nil
    }
    guard let trackId = int64Value(value) else {
      throw ErikaPluginError.invalidArguments("trackId must be an integer or null.")
    }
    return trackId >= 0 ? trackId : nil
  }

  private func optionalReadAheadBytes(_ value: Any?) throws -> UInt64 {
    try optionalByteCount(value, name: "httpReadAheadBytes")
  }

  private func optionalBackBufferBytes(_ value: Any?) throws -> UInt64 {
    try optionalByteCount(value, name: "httpBackBufferBytes")
  }

  private func optionalByteCount(_ value: Any?, name: String) throws -> UInt64 {
    if value == nil || value is NSNull {
      return 0
    }
    guard let number = value as? NSNumber else {
      throw ErikaPluginError.invalidArguments("\(name) must be a non-negative integer.")
    }
    let numericValue = number.doubleValue
    guard numericValue.isFinite,
          numericValue >= 0,
          numericValue.rounded(.towardZero) == numericValue,
          numericValue <= Double(Int64.max) else {
      throw ErikaPluginError.invalidArguments("\(name) must be a non-negative integer.")
    }
    return number.uint64Value
  }

  private func danmakuConfig(
    from args: [String: Any],
    base: ErikaDanmakuConfigC
  ) -> ErikaDanmakuConfigC {
    var config = base
    if let value = boolValue(args["enabled"]) {
      config.enabled = value ? 1 : 0
    }
    if let value = doubleValue(args["fontSize"]) {
      config.fontSize = Float(value)
    }
    if let value = doubleValue(args["opacity"]) {
      config.opacity = Float(value)
    }
    if let value = doubleValue(args["displayArea"]) {
      config.displayArea = Float(value)
    }
    if let value = doubleValue(args["scrollDurationSeconds"]) {
      config.scrollDurationSeconds = Float(value)
    }
    if let value = doubleValue(args["scrollSpeedFactor"]) {
      config.scrollSpeedFactor = Float(value)
    }
    if let value = doubleValue(args["trackGapRatio"]) {
      config.trackGapRatio = Float(value)
    }
    if let value = doubleValue(args["outlineWidth"]) {
      config.outlineWidth = Float(value)
    }
    if let value = doubleValue(args["shadowOffsetX"]) {
      config.shadowOffsetX = Float(value)
    }
    if let value = doubleValue(args["shadowOffsetY"]) {
      config.shadowOffsetY = Float(value)
    }
    if let value = boolValue(args["mergeDuplicates"]) {
      config.mergeDuplicates = value ? 1 : 0
    }
    if let value = boolValue(args["allowStacking"]) {
      config.allowStacking = value ? 1 : 0
    }
    if let value = boolValue(args["allowScrollOverwrite"]) {
      config.allowScrollOverwrite = value ? 1 : 0
    }
    if let value = int64Value(args["maxQuantity"]), value > 0 {
      config.maxQuantity = UInt32(clamping: value)
    }
    if let value = int64Value(args["maxLinesPerMode"]), value > 0 {
      config.maxLinesPerMode = UInt32(clamping: value)
    }
    if let value = boolValue(args["blockTop"]) {
      config.blockTop = value ? 1 : 0
    }
    if let value = boolValue(args["blockBottom"]) {
      config.blockBottom = value ? 1 : 0
    }
    if let value = boolValue(args["blockScroll"]) {
      config.blockScroll = value ? 1 : 0
    }
    if let value = int64Value(args["shadowStyle"]) {
      config.shadowStyle = Int32(clamping: value)
    }
    return config
  }

  private func startPollTimerIfNeeded() {
    guard pollTimer == nil else {
      return
    }
    let timer = Timer(timeInterval: 0.05, repeats: true) { [weak self] _ in
      guard let self else {
        return
      }
      let sink = Self.sharedEventSink
      for host in self.players.values {
        host.pollEvents(sendEvent: sink)
      }
    }
    // Event delivery does not require a hard 50 ms deadline. Let macOS
    // coalesce this maintenance poll with nearby UI/audio work.
    timer.tolerance = 0.01
    pollTimer = timer
    RunLoop.main.add(timer, forMode: .common)
  }

  private func stopPollTimerIfIdle() {
    guard players.isEmpty else {
      return
    }
    pollTimer?.invalidate()
    pollTimer = nil
  }

  private func dictionaryArgs(_ arguments: Any?) throws -> [String: Any] {
    guard let args = arguments as? [String: Any] else {
      throw ErikaPluginError.invalidArguments("Arguments must be a dictionary.")
    }
    return args
  }

  private func int64Value(_ value: Any?) -> Int64? {
    if let value = value as? Int64 {
      return value
    }
    if let value = value as? NSNumber {
      return value.int64Value
    }
    if let value = value as? String {
      return Int64(value)
    }
    return nil
  }

  private func doubleValue(_ value: Any?) -> Double? {
    if let value = value as? Double {
      return value
    }
    if let value = value as? NSNumber {
      return value.doubleValue
    }
    if let value = value as? String {
      return Double(value)
    }
    return nil
  }

  private func boolValue(_ value: Any?) -> Bool? {
    if let value = value as? Bool {
      return value
    }
    if let value = value as? NSNumber {
      return value.boolValue
    }
    if let value = value as? String {
      switch value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
      case "1", "true", "yes", "on":
        return true
      case "0", "false", "no", "off":
        return false
      default:
        return nil
      }
    }
    return nil
  }

  private func requiredInt64(_ value: Any?, name: String) throws -> Int64 {
    if let value = value as? Int64 {
      return value
    }
    if let value = value as? NSNumber {
      return value.int64Value
    }
    if let value = value as? String, let parsed = Int64(value) {
      return parsed
    }
    throw ErikaPluginError.invalidArguments("\(name) is required.")
  }

  private func requiredUInt64(_ value: Any?, name: String) throws -> UInt64 {
    if let value = value as? UInt64 {
      return value
    }
    if let value = value as? Int64, value >= 0 {
      return UInt64(value)
    }
    if let value = value as? NSNumber {
      return value.uint64Value
    }
    if let value = value as? String, let parsed = UInt64(value) {
      return parsed
    }
    throw ErikaPluginError.invalidArguments("\(name) is required.")
  }

  /// Runs `work` on `queue`, then answers on the main thread, which is where
  /// Flutter wants results. `keeping` stays referenced until the answer, so
  /// the last reference to a player never drops on a background queue (its
  /// teardown touches the display driver).
  private func respondOffMain<T>(
    queue: DispatchQueue,
    keeping owner: AnyObject?,
    result: @escaping FlutterResult,
    work: @escaping () throws -> T
  ) {
    queue.async {
      let outcome = Result { try work() }
      DispatchQueue.main.async {
        withExtendedLifetime(owner) {}
        switch outcome {
        case .success(let value):
          result(value)
        case .failure(let error):
          result(self.flutterError(error))
        }
      }
    }
  }

  private func diagnosticsCount(_ value: Any?, name: String, defaultValue: UInt64) throws -> UInt64 {
    if value == nil || value is NSNull {
      return defaultValue
    }
    if let number = value as? NSNumber, number.int64Value >= 0 {
      return number.uint64Value
    }
    throw ErikaPluginError.invalidArguments("\(name) must be a non-negative integer.")
  }

  private func diagnosticsLogLevel(_ value: Any?) throws -> Int32 {
    if value == nil || value is NSNull {
      return -1
    }
    let names = ["trace", "debug", "info", "warn", "error"]
    if let name = value as? String {
      let lowered = name.lowercased()
      if let index = names.firstIndex(of: lowered == "warning" ? "warn" : lowered) {
        return Int32(index)
      }
    } else if let number = value as? NSNumber, (0...4).contains(number.intValue) {
      return Int32(number.intValue)
    }
    throw ErikaPluginError.invalidArguments("minLevel must be one of \(names.joined(separator: ", ")).")
  }

  private func flutterError(_ error: Error) -> FlutterError {
    if let pluginError = error as? ErikaPluginError,
       case .diagnosticsUnsupported = pluginError {
      return FlutterError(code: "ERIKA_UNSUPPORTED", message: String(describing: error), details: nil)
    }
    return FlutterError(
      code: "ERIKA_ERROR",
      message: String(describing: error),
      details: nil
    )
  }
}
