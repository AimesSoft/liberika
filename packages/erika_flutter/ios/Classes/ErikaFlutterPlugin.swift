import Darwin
import AVFoundation
import Flutter
import MediaPlayer
import Metal
import ObjectiveC.runtime
import QuartzCore
import UIKit

private let erikaWindowHostedVideoSurfaceId: Int64 = -1
private let erikaDebugLabelsEnabled =
  ProcessInfo.processInfo.environment["ERIKA_DEBUG_LABELS"] == "1"

private func erikaHdrWrite(_ message: String) {
  fputs("ErikaHDR[iOS]: \(message)\n", stderr)
  fflush(stderr)
}

private func erikaHdrLog(_ enabled: Bool, _ message: String) {
  if enabled {
    erikaHdrWrite(message)
  }
}

private func erikaHdrEnvironmentEnabled() -> Bool {
  guard let value = ProcessInfo.processInfo.environment["ERIKA_HDR_DEBUG"] else {
    return false
  }
  switch value.lowercased() {
  case "1", "true", "yes", "on":
    return true
  default:
    return false
  }
}

private func erikaOutputModeLabel(_ config: ErikaPresenterConfigC) -> String {
  switch config.outputMode {
  case 1:
    return String(format: "AppleEdr(headroom=%.2f)", config.edrHeadroom)
  case 2:
    return String(format: "ExtendedLinear(headroom=%.2f)", config.edrHeadroom)
  case 3:
    return String(format: "Auto(headroom=%.2f)", config.edrHeadroom)
  default:
    return "Sdr"
  }
}

private func erikaScreenSummary(_ screen: UIScreen?) -> String {
  guard let screen else {
    return "screen=nil"
  }
  var parts = [
    "scale=\(screen.scale)",
    "nativeScale=\(screen.nativeScale)",
    "brightness=\(String(format: "%.3f", screen.brightness))",
    "gamut=\(screen.traitCollection.displayGamut.rawValue)",
  ]
  if #available(iOS 16.0, *) {
    parts.append("currentEDR=\(String(format: "%.3f", screen.currentEDRHeadroom))")
    parts.append("potentialEDR=\(String(format: "%.3f", screen.potentialEDRHeadroom))")
  }
  return parts.joined(separator: " ")
}

private func erikaLayerValue(_ layer: CAMetalLayer, selector name: String) -> String {
  let selector = Selector(name)
  guard layer.responds(to: selector) else {
    return "unavailable"
  }
  return String(describing: layer.value(forKey: name) ?? "nil")
}

private func erikaConfigureLayerDynamicRange(_ layer: CAMetalLayer, config: ErikaPresenterConfigC) {
  if config.outputMode == 1 || config.outputMode == 2 {
    layer.contentsFormat = .RGBA16Float
    if #available(iOS 16.0, *) {
      layer.wantsExtendedDynamicRangeContent = true
      layer.edrMetadata = CAEDRMetadata.hdr10(
        minLuminance: 0.02,
        maxLuminance: 1200.0,
        opticalOutputScale: 203.0
      )
    }
    if #available(iOS 18.0, *) {
      layer.toneMapMode = .ifSupported
    }
    if #available(iOS 26.0, *) {
      layer.preferredDynamicRange = .high
      layer.contentsHeadroom = CGFloat(max(config.edrHeadroom, 1.0))
    }
    return
  }

  layer.contentsFormat = .RGBA8Uint
  if #available(iOS 16.0, *) {
    layer.wantsExtendedDynamicRangeContent = false
    layer.edrMetadata = nil
  }
  if #available(iOS 18.0, *) {
    layer.toneMapMode = .automatic
  }
  // Auto is initialized in SDR, but the native renderer owns later changes.
  // Do not pin preferredDynamicRange/contentsHeadroom to SDR here.
  if config.outputMode != 3 {
    if #available(iOS 26.0, *) {
      layer.preferredDynamicRange = .standard
      layer.contentsHeadroom = 0.0
    }
  }
}

private func erikaLayerSummary(_ layer: CAMetalLayer) -> String {
  let wantsEDR: String
  if #available(iOS 16.0, *) {
    wantsEDR = String(layer.wantsExtendedDynamicRangeContent)
  } else {
    wantsEDR = "unavailable"
  }
  let toneMapMode: String
  if #available(iOS 18.0, *) {
    toneMapMode = String(describing: layer.toneMapMode)
  } else {
    toneMapMode = "unavailable"
  }
  let preferredDynamicRange: String
  if #available(iOS 26.0, *) {
    preferredDynamicRange = String(describing: layer.preferredDynamicRange)
  } else {
    preferredDynamicRange = "unavailable"
  }
  let contentsHeadroom: String
  if #available(iOS 26.0, *) {
    contentsHeadroom = String(format: "%.3f", layer.contentsHeadroom)
  } else {
    contentsHeadroom = "unavailable"
  }
  let colorSpace = layer.colorspace?.name.map { String(describing: $0) } ?? "nil"
  return [
    "pixelFormat=\(layer.pixelFormat.rawValue)",
    "drawable=\(Int(layer.drawableSize.width))x\(Int(layer.drawableSize.height))",
    "framebufferOnly=\(layer.framebufferOnly)",
    "opaque=\(layer.isOpaque)",
    "contentsFormat=\(layer.contentsFormat)",
    "wantsEDR=\(wantsEDR)",
    "toneMapMode=\(toneMapMode)",
    "preferredDynamicRange=\(preferredDynamicRange)",
    "contentsHeadroom=\(contentsHeadroom)",
    "edrMetadata=\(erikaLayerValue(layer, selector: "EDRMetadata"))",
    "colorspace=\(colorSpace)",
  ].joined(separator: " ")
}

private struct ErikaTrackSelectionC {
  var video: Int64 = -1
  var audio: Int64 = -1
  var subtitle: Int64 = -1
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
  var startPositionMicros: UInt64 = 0
  var reserved: UInt64 = 0
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

private let erikaDefaultDisplayFps = 60

private struct ErikaDanmakuConfigC {
  var enabled: UInt8 = 1
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
      return "Unable to load Erika C ABI. Tried: \(paths.joined(separator: ", "))"
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
  typealias CreateFn = @convention(c) () -> UnsafeMutableRawPointer?
  typealias CreateWithOutputModeFn = @convention(c) (Int32, Float) -> UnsafeMutableRawPointer?
  typealias DestroyFn = @convention(c) (UnsafeMutableRawPointer?) -> Void
  typealias OpenFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Int32
  typealias OpenWithHeadersFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafeRawPointer?, UInt) -> Int32
  typealias OpenWithOptionsFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafeRawPointer?) -> Int32
  typealias CommandFn = @convention(c) (UnsafeMutableRawPointer?) -> Int32
  typealias SeekFn = @convention(c) (UnsafeMutableRawPointer?, UInt64) -> Int32
  typealias SetPlaybackRateFn = @convention(c) (UnsafeMutableRawPointer?, Double) -> Int32
  typealias SetVolumeFn = @convention(c) (UnsafeMutableRawPointer?, Double) -> Int32
  typealias SetUpscalerFn = @convention(c) (UnsafeMutableRawPointer?, Int32) -> Int32
  typealias LoadUpscalerModelFn = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafePointer<UInt8>?, UInt) -> Int32
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
  typealias ResizeSurfaceFn = @convention(c) (UnsafeMutableRawPointer?, UInt32, UInt32, Double) -> Int32
  typealias RenderTickFn = @convention(c) (UnsafeMutableRawPointer?, Double, UnsafeMutableRawPointer?) -> Int32
  typealias RenderTickWithTimingFn = @convention(c) (UnsafeMutableRawPointer?, Double, UnsafePointer<Double>?, UnsafeMutableRawPointer?) -> Int32
  typealias AudioOnlyTickFn = @convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Int32
  typealias CaptureFrameRgbaFn = @convention(c) (UnsafeMutableRawPointer?, UInt32, UInt32, UnsafeMutableRawPointer?, Int) -> Int32
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
  let loadUpscalerModel: LoadUpscalerModelFn?
  let clearUpscalerModel: CommandFn?
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
  let resizeSurface: ResizeSurfaceFn
  let detachSurface: CommandFn
  let renderTick: RenderTickFn
  let renderTickWithTiming: RenderTickWithTimingFn?
  let audioOnlyTick: AudioOnlyTickFn
  let captureFrameRgba: CaptureFrameRgbaFn?
  let pollEvent: PollEventFn
  let lastErrorMessage: LastErrorMessageFn
  let stringFree: StringFreeFn
  let getDiagnosticsJson: GetDiagnosticsJsonFn?
  let logReadJson: LogReadJsonFn?
  let invokeJson: InvokeJsonFn?
  private let globalJsonMethodsLock = NSLock()
  private var globalJsonMethods: Set<String>?

  private let libraryHandle: UnsafeMutableRawPointer
  let path: String

  private init() throws {
    let loaded = try Self.openLibrary()
    libraryHandle = loaded.handle
    path = loaded.path
    erikaHdrLog(
      erikaHdrEnvironmentEnabled(),
      "loaded native library from \(path)"
    )

    create = try Self.load("erika_presenter_create", from: libraryHandle, as: CreateFn.self)
    createWithOutputMode = Self.loadOptional("erika_presenter_create_with_output_mode", from: libraryHandle, as: CreateWithOutputModeFn.self)
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
    loadUpscalerModel = Self.loadOptional("erika_presenter_load_upscaler_model", from: libraryHandle, as: LoadUpscalerModelFn.self)
    clearUpscalerModel = Self.loadOptional("erika_presenter_clear_upscaler_model", from: libraryHandle, as: CommandFn.self)
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
    resizeSurface = try Self.load("erika_presenter_resize_surface", from: libraryHandle, as: ResizeSurfaceFn.self)
    detachSurface = try Self.load("erika_presenter_detach_surface", from: libraryHandle, as: CommandFn.self)
    renderTick = try Self.load("erika_presenter_render_tick", from: libraryHandle, as: RenderTickFn.self)
    renderTickWithTiming = Self.loadOptional("erika_presenter_render_tick_with_timing", from: libraryHandle, as: RenderTickWithTimingFn.self)
    audioOnlyTick = try Self.load("erika_presenter_audio_only_tick", from: libraryHandle, as: AudioOnlyTickFn.self)
    captureFrameRgba = Self.loadOptional("erika_presenter_capture_frame_rgba", from: libraryHandle, as: CaptureFrameRgbaFn.self)
    pollEvent = try Self.load("erika_presenter_poll_event", from: libraryHandle, as: PollEventFn.self)
    lastErrorMessage = try Self.load("erika_last_error_message", from: libraryHandle, as: LastErrorMessageFn.self)
    stringFree = try Self.load("erika_string_free", from: libraryHandle, as: StringFreeFn.self)
    getDiagnosticsJson = Self.loadOptional("erika_presenter_get_diagnostics_json", from: libraryHandle, as: GetDiagnosticsJsonFn.self)
    logReadJson = Self.loadOptional("erika_log_read_json", from: libraryHandle, as: LogReadJsonFn.self)
    invokeJson = Self.loadOptional("erika_presenter_invoke_json", from: libraryHandle, as: InvokeJsonFn.self)
  }

  private static func openLibrary() throws -> (handle: UnsafeMutableRawPointer, path: String) {
    var failures: [ErikaPluginError] = []
    if let handle = dlopen(nil, RTLD_NOW), dlsym(handle, "erika_presenter_create") != nil {
      return (handle, "main executable")
    }

    var candidates: [String] = []
    let environment = ProcessInfo.processInfo.environment
    if let override = environment["ERIKA_CAPI_DYLIB"], !override.isEmpty {
      candidates.append(override)
    }
    let bundle = Bundle(for: ErikaFlutterPlugin.self)
    if let pluginExecutable = bundle.executablePath {
      candidates.append(pluginExecutable)
    }
    if let resourcePath = bundle.path(forResource: "liberika_capi", ofType: "dylib") {
      candidates.append(resourcePath)
    }
    if let frameworksPath = Bundle.main.privateFrameworksPath {
      candidates.append(URL(fileURLWithPath: frameworksPath).appendingPathComponent("liberika_capi.dylib").path)
    }
    if let executablePath = Bundle.main.executablePath {
      let executableDirectory = URL(fileURLWithPath: executablePath).deletingLastPathComponent().path
      candidates.append(URL(fileURLWithPath: executableDirectory).appendingPathComponent("liberika_capi.dylib").path)
    }

    for path in candidates {
      if let handle = dlopen(path, RTLD_NOW | RTLD_LOCAL) {
        if dlsym(handle, "erika_presenter_create") != nil {
          return (handle, path)
        }
        dlclose(handle)
        failures.append(.libraryLoadFailed(path, "erika_presenter_create not found"))
        continue
      }
      let detail = dlerror().map { String(cString: $0) }
      failures.append(.libraryLoadFailed(path, detail))
    }
    throw ErikaPluginError.libraryNotFound(failures.map(String.init(describing:)))
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
    if let createWithOutputMode {
      return createWithOutputMode(config.outputMode, config.edrHeadroom)
    }
    return create()
  }

  /// Copies and frees a JSON string returned by a diagnostics function.
  func takeJsonString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
    guard let pointer else {
      return nil
    }
    defer { stringFree(pointer) }
    return String(validatingUTF8: pointer)
  }

  /// Whether a JSON table method takes no presenter (scope `global`), read
  /// once from the table itself so the plugin keeps no copy of it. Global
  /// methods receive a NULL handle; invocation still stays on the caller's
  /// presenter owner so command order is preserved.
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
}

private enum ErikaSurfaceLeaseKey { static var value: UInt8 = 0 }

private extension CAMetalLayer {
  var erikaOwnerLease: ErikaSurfaceLease? {
    get { objc_getAssociatedObject(self, &ErikaSurfaceLeaseKey.value) as? ErikaSurfaceLease }
    set { objc_setAssociatedObject(self, &ErikaSurfaceLeaseKey.value, newValue, .OBJC_ASSOCIATION_RETAIN_NONATOMIC) }
  }
}

private final class ErikaPlayerHost {
  let id: Int64

  private var library: ErikaNativeLibrary! // owner only
  private var creationError: Error?
  private var handle: UnsafeMutableRawPointer!
  let owner: ErikaPresenterOwner
  private var closed = false
  private var surfaceLease: ErikaSurfaceLease? // main only
  private var nativeLease: ErikaSurfaceLease? // owner only
  private var nativeLayer: CAMetalLayer?
  private var surfaceError: Error?
  private weak var attachedView: ErikaMetalSurfaceView?
  private var attachedViewId: Int64?
  private var displayLink: CADisplayLink?
  private var displayLinkProxy: DisplayLinkProxy?
  @ErikaSnapshot private var startTimeSeconds: CFTimeInterval = CACurrentMediaTime()
  @ErikaSnapshot private var currentDanmakuConfig = ErikaDanmakuConfigC()
  private let hdrDebug: Bool
  private let presenterConfig: ErikaPresenterConfigC
  private let allowBackgroundPlayback: Bool
  private var loggedRenderThread = false
  private var loggedFirstRenderedVideoFrame = false
  private var latestPresenterStats = ErikaPresenterStatsC()
  private var fallbackTimer: DispatchSourceTimer?
  private var isAppInBackground = false
  @ErikaSnapshot private(set) var nowPlayingTitle = ""
  @ErikaSnapshot private(set) var nowPlayingArtist: String?
  @ErikaSnapshot private(set) var nowPlayingAlbum: String?
  private(set) var nowPlayingArtwork: MPMediaItemArtwork?
  @ErikaSnapshot private(set) var durationSeconds: Double?
  @ErikaSnapshot private(set) var positionSeconds = 0.0
  @ErikaSnapshot private(set) var playbackRate = 1.0
  @ErikaSnapshot private(set) var isPlaying = false
  var onNowPlayingChanged: ((ErikaPlayerHost) -> Void)?

  init(
    id: Int64,
    config: ErikaPresenterConfigC,
    hdrDebug: Bool,
    allowBackgroundPlayback: Bool
  ) throws {
    self.id = id
    owner = ErikaPresenterOwner(id: id)
    self.hdrDebug = hdrDebug
    self.allowBackgroundPlayback = allowBackgroundPlayback
    presenterConfig = config
    owner.post("create") { [self] in
      guard let library = ErikaNativeLibrary.shared else {
        creationError = ErikaPluginError.libraryNotFound(["main executable", "ERIKA_CAPI_DYLIB", "app bundle"])
        return
      }
      self.library = library
      handle = library.createPresenter(config: config)
    }

  }

  /// Main owns driver/UI bookkeeping; native state is released only after
  /// all accepted commands have drained. No join or main.sync is necessary.
  func shutdown(completion: (() -> Void)? = nil) {
    precondition(Thread.isMainThread)
    guard !closed else { completion?(); return }
    closed = true
    displayLink?.invalidate()
    displayLink = nil
    displayLinkProxy = nil
    fallbackTimer?.cancel()
    fallbackTimer = nil
    attachedView?.attachedPlayerId = nil
    attachedView = nil
    attachedViewId = nil
    surfaceLease = nil
    onNowPlayingChanged = nil
    nowPlayingArtwork = nil
    owner.finish { [self] in
      if let handle {
        _ = library.detachSurface(handle)
        library.destroy(handle)
        self.handle = nil
      }
      nativeLease?.complete()
      nativeLease = nil
      let layer = nativeLayer
      nativeLayer = nil
      DispatchQueue.main.async {
        withExtendedLifetime(layer) {}
        completion?()
      }
    }
  }

  func enqueue(label: String = "command", result: @escaping FlutterResult, work: @escaping () throws -> Any?) {
    let accepted = owner.post(label) { [self] in
      let outcome: Any?
      do {
        guard handle != nil else { throw creationError ?? ErikaPluginError.presenterCreateFailed }
        outcome = try work()
      } catch {
        let unsupported: Bool
        if let pluginError = error as? ErikaPluginError, case .diagnosticsUnsupported = pluginError { unsupported = true }
        else { unsupported = false }
        outcome = FlutterError(code: unsupported ? "ERIKA_UNSUPPORTED" : "ERIKA_ERROR", message: String(describing: error), details: nil)
      }
      DispatchQueue.main.async { result(outcome) }
    }
    if !accepted { result(FlutterError(code: "ERIKA_ERROR", message: "Presenter disposed", details: nil)) }
  }

  func enqueue(operations: [() throws -> Any?], result: @escaping FlutterResult, label: String) {
    enqueue(label: label, result: result) {
      var value: Any?
      for operation in operations { value = try operation() }
      return value
    }
  }

  func created(result: @escaping FlutterResult) {
    enqueue(result: result) { [self] in id }
  }

  private func withNativeCall<T>(_ operation: () throws -> T) rethrows -> T {
    owner.assertCurrent()
    return try operation()
  }

  private func claimSurface(previous: ErikaSurfaceLease?) -> (lease: ErikaSurfaceLease, dependency: ErikaSurfaceLease?) {
    precondition(Thread.isMainThread)
    if let previous, let oldHost = previous.owner as? ErikaPlayerHost,
       oldHost !== self, oldHost.surfaceLease === previous {
      oldHost.detach(viewId: nil)
    }
    let dependency = previous?.owner === self ? nil : previous
    let lease = ErikaSurfaceLease(owner: self)
    surfaceLease = lease
    return (lease, dependency)
  }

  func completeSurface(result: @escaping FlutterResult, value: Any? = nil) {
    enqueue(result: result) { [self] in
      if let error = surfaceError { surfaceError = nil; throw error }
      return value
    }
  }

  /// If detach fails, destroy before releasing a surface lease. Otherwise a
  /// new owner could attach a layer still referenced by the failed presenter.
  private func detachNativeSurface() {
    owner.assertCurrent()
    guard let handle else { return }
    let status = library.detachSurface(handle)
    if status != 0 {
      surfaceError = ErikaPluginError.erikaStatus("detach_surface", status)
      library.destroy(handle)
      self.handle = nil
    }
  }

  private func enqueueDetach() {
    owner.cancelFrame()
    owner.post { [self] in
      guard handle != nil else { return }
      detachNativeSurface()
      nativeLease?.complete()
      nativeLease = nil
      let layer = nativeLayer
      nativeLayer = nil
      DispatchQueue.main.async {
        withExtendedLifetime(layer) {}
      }
    }
  }

  func open(
    uri: String,
    httpHeaders: [String: String],
    httpReadAheadBytes: UInt64 = 0,
    httpBackBufferBytes: UInt64 = 0
  ) throws {
    owner.assertCurrent()
    isPlaying = false
    positionSeconds = 0
    durationSeconds = nil
    if nowPlayingTitle.isEmpty {
      let fallbackTitle = URL(string: uri)?.lastPathComponent.removingPercentEncoding
        ?? URL(fileURLWithPath: uri).lastPathComponent
      nowPlayingTitle = fallbackTitle.isEmpty ? "Erika" : fallbackTitle
    }
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
        notifyNowPlayingChanged()
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
    notifyNowPlayingChanged()
  }

  func play() throws {
    if !owner.isCurrent {
      try prepareAudioSession()
      owner.post { [self] in
        guard handle != nil else { return }
        do { try play() } catch { NSLog("Erika: remote play failed: \(error)") }
      }
      return
    }

    owner.assertCurrent()
    try check(library.play(handle), operation: "play")
    isPlaying = true
    updateTickDriver()
    notifyNowPlayingChanged()
  }
  func pause() throws {
    if !owner.isCurrent {
      owner.post { [self] in
        guard handle != nil else { return }
        do { try pause() } catch { NSLog("Erika: remote pause failed: \(error)") }
      }
      return
    }

    owner.assertCurrent()
    try check(library.pause(handle), operation: "pause")
    isPlaying = false
    updateTickDriver()
    notifyNowPlayingChanged()
  }
  func stop() throws {
    if !owner.isCurrent {
      owner.post { [self] in
        guard handle != nil else { return }
        do { try stop() } catch { NSLog("Erika: remote stop failed: \(error)") }
      }
      return
    }

    owner.assertCurrent()
    try check(library.stop(handle), operation: "stop")
    isPlaying = false
    positionSeconds = 0
    updateTickDriver()
    notifyNowPlayingChanged()
  }
  func close() throws {
    if !owner.isCurrent {
      owner.post { [self] in
        guard handle != nil else { return }
        do { try close() } catch { NSLog("Erika: remote close failed: \(error)") }
      }
      return
    }

    owner.assertCurrent()
    try check(library.close(handle), operation: "close")
    isPlaying = false
    positionSeconds = 0
    durationSeconds = nil
    updateTickDriver()
    notifyNowPlayingChanged()
  }

  func seek(positionMicros: UInt64) throws {
    if !owner.isCurrent {
      owner.post { [self] in
        guard handle != nil else { return }
        do { try seek(positionMicros: positionMicros) } catch { NSLog("Erika: remote seek failed: \(error)") }
      }
      return
    }

    owner.assertCurrent()
    try check(library.seek(handle, positionMicros), operation: "seek")
    positionSeconds = Double(positionMicros) / 1_000_000
    notifyNowPlayingChanged()
  }

  func setPlaybackRate(_ rate: Double) throws {
    owner.assertCurrent()
    guard let setRate = library.setPlaybackRate else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_playback_rate")
    }
    try check(setRate(handle, rate), operation: "set_playback_rate")
    playbackRate = rate
    notifyNowPlayingChanged()
  }

  func setMediaMetadata(title: String, artist: String?, album: String?, artworkData: Data?) throws {
    let artwork: MPMediaItemArtwork?
    if let artworkData {
      guard let image = UIImage(data: artworkData) else {
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
    notifyNowPlayingChanged()
  }

  func clearMediaMetadata() {
    nowPlayingTitle = ""
    nowPlayingArtist = nil
    nowPlayingAlbum = nil
    nowPlayingArtwork = nil
    notifyNowPlayingChanged()
  }

  func setAppInBackground(_ value: Bool) {
    isAppInBackground = value
    updateTickDriver()
  }

  func prepareForInactiveApp() {
    setAppInBackground(true)
    scheduleTick(audioOnly: true)
  }

  func didEnterBackground() {
    setAppInBackground(true)
    if !allowBackgroundPlayback {
      // An accepted play may still be behind a slow open. Decide on the
      // owner after earlier commands, rather than using main's old snapshot.
      owner.post("backgroundPause") { [self] in
        guard handle != nil, isPlaying else { return }
        try? pause()
      }
    }
  }

  func setVolume(_ volume: Double) throws {
    owner.assertCurrent()
    guard let setVolume = library.setVolume else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_volume")
    }
    let clampedVolume = volume.isFinite ? min(max(volume, 0.0), 1.0) : 1.0
    try check(setVolume(handle, clampedVolume), operation: "set_volume")
  }

  func setUpscaler(mode: Int32) throws {
    owner.assertCurrent()
    guard let setUpscaler = library.setUpscaler else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_upscaler")
    }
    try check(setUpscaler(handle, mode), operation: "set_upscaler")
  }

  func loadUpscalerModel(name: String, data: Data) throws {
    owner.assertCurrent()
    guard let load = library.loadUpscalerModel else {
      throw ErikaPluginError.symbolMissing("erika_presenter_load_upscaler_model")
    }
    let status = name.withCString { namePointer in
      data.withUnsafeBytes { bytes in
        load(handle, namePointer, bytes.bindMemory(to: UInt8.self).baseAddress, UInt(data.count))
      }
    }
    try check(status, operation: "load_upscaler_model")
  }

  func clearUpscalerModel() throws {
    owner.assertCurrent()
    guard let clear = library.clearUpscalerModel else {
      throw ErikaPluginError.symbolMissing("erika_presenter_clear_upscaler_model")
    }
    try check(clear(handle), operation: "clear_upscaler_model")
  }

  func setSubtitleScale(_ scale: Double) throws {
    owner.assertCurrent()
    guard let setSubtitleScale = library.setSubtitleScale else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_scale")
    }
    let clampedScale = scale.isFinite ? min(max(scale, 0.25), 4.0) : 1.0
    try check(setSubtitleScale(handle, clampedScale), operation: "set_subtitle_scale")
  }

  func setSubtitleFont(family: String?, filePath: String?) throws {
    owner.assertCurrent()
    guard let setSubtitleFont = library.setSubtitleFont else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_font")
    }
    let status = withOptionalCString(family ?? "") { familyCString in
      withOptionalCString(filePath ?? "") { filePathCString in
        setSubtitleFont(handle, familyCString, filePathCString)
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
    owner.assertCurrent()
    guard let setSubtitleStyle = library.setSubtitleStyle else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_subtitle_style")
    }
    let status = withOptionalCString(fontFamily ?? "") { fontFamilyCString in
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
    try check(status, operation: "set_subtitle_style")
  }

  func upscalerStatus() throws -> [String: Any] {
    owner.assertCurrent()
    guard let getStatus = library.getUpscalerStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_upscaler_status")
    }
    var status = ErikaUpscalerStatusC()
    let result = withUnsafeMutablePointer(to: &status) { pointer in
      getStatus(handle, UnsafeMutableRawPointer(pointer))
    }
    try check(result, operation: "get_upscaler_status")
    return status.toFlutterMap()
  }

  func registerSubtitleMemoryFont(_ data: Data) throws -> UInt64 {
    owner.assertCurrent()
    guard let register = library.registerSubtitleMemoryFont else {
      throw ErikaPluginError.symbolMissing("erika_presenter_register_subtitle_memory_font")
    }
    var fontId: UInt64 = 0
    let status = data.withUnsafeBytes { bytes in
      register(handle, bytes.bindMemory(to: UInt8.self).baseAddress, UInt(data.count), &fontId)
    }
    try check(status, operation: "register_subtitle_memory_font")
    return fontId
  }

  func selectSubtitleMemoryFonts(_ fontIds: [UInt64]) throws {
    owner.assertCurrent()
    guard let select = library.selectSubtitleMemoryFonts else {
      throw ErikaPluginError.symbolMissing("erika_presenter_select_subtitle_memory_fonts")
    }
    try fontIds.withUnsafeBufferPointer { buffer in
      try check(select(handle, buffer.baseAddress, UInt(buffer.count)), operation: "select_subtitle_memory_fonts")
    }
  }

  func clearSubtitleMemoryFonts() throws {
    owner.assertCurrent()
    guard let clear = library.clearSubtitleMemoryFonts else {
      throw ErikaPluginError.symbolMissing("erika_presenter_clear_subtitle_memory_fonts")
    }
    try check(clear(handle), operation: "clear_subtitle_memory_fonts")
  }

  func subtitleMemoryFontStatus() throws -> [String: Any] {
    owner.assertCurrent()
    guard let getStatus = library.getSubtitleMemoryFontStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_subtitle_memory_font_status")
    }
    var status = ErikaSubtitleMemoryFontStatusC()
    defer {
      if let freeStatus = library.freeSubtitleMemoryFontStatus {
        withUnsafeMutablePointer(to: &status) { freeStatus(UnsafeMutableRawPointer($0)) }
      }
    }
    try withUnsafeMutablePointer(to: &status) { pointer in
      try check(getStatus(handle, UnsafeMutableRawPointer(pointer)), operation: "get_subtitle_memory_font_status")
    }
    return [
      "registeredCount": Int(status.registeredCount),
      "registeredBytes": Int(status.registeredBytes),
      "selectedCount": Int(status.selectedCount),
      "generation": Int64(clamping: status.generation),
      "selectedIds": status.selectedIds.map { pointer in
        (0..<Int(status.selectedCount)).map { Int64(clamping: pointer[$0]) }
      } ?? [],
    ]
  }

  func outputStatus() throws -> [String: Any] {
    owner.assertCurrent()
    guard let getStatus = library.getOutputStatus else {
      throw ErikaPluginError.symbolMissing("erika_presenter_get_output_status")
    }
    var status = ErikaOutputStatusC()
    let result = withUnsafeMutablePointer(to: &status) { pointer in
      getStatus(handle, UnsafeMutableRawPointer(pointer))
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
    // Global methods use a NULL presenter, but still execute on this owner
    // to preserve method-channel command order.
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
    owner.assertCurrent()
    return latestPresenterStats.toFlutterMap()
  }

  func addExternalSubtitle(uri: String) throws -> Int64 {
    owner.assertCurrent()
    var trackId: Int64 = 0
    try uri.withCString { cString in
      try check(library.addExternalSubtitle(handle, cString, &trackId), operation: "add_external_subtitle")
    }
    return trackId
  }

  func removeSubtitleTrack(trackId: Int64) throws {
    owner.assertCurrent()
    try check(library.removeSubtitleTrack(handle, trackId), operation: "remove_subtitle_track")
  }

  func loadDanmakuFile(uri: String) throws {
    owner.assertCurrent()
    guard let load = library.loadDanmakuFile else {
      throw ErikaPluginError.symbolMissing("erika_presenter_load_danmaku_file")
    }
    try uri.withCString { cString in
      try check(load(handle, cString), operation: "load_danmaku_file")
    }
  }

  func loadDanmakuJson(_ json: String) throws {
    owner.assertCurrent()
    guard let load = library.loadDanmakuJson else {
      throw ErikaPluginError.symbolMissing("erika_presenter_load_danmaku_json")
    }
    try json.withCString { cString in
      try check(load(handle, cString), operation: "load_danmaku_json")
    }
  }

  func addDanmakuTrackFile(uri: String, name: String?, offsetMicros: Int64) throws -> UInt64 {
    owner.assertCurrent()
    guard let add = library.addDanmakuTrackFile else {
      throw ErikaPluginError.symbolMissing("erika_presenter_add_danmaku_track_file")
    }
    var trackId: UInt64 = 0
    let status = uri.withCString { uriCString in
      withOptionalCString(name) { nameCString in
        add(handle, uriCString, nameCString, offsetMicros, &trackId)
      }
    }
    try check(status, operation: "add_danmaku_track_file")
    return trackId
  }

  func addDanmakuTrackJson(_ json: String, name: String?, offsetMicros: Int64) throws -> UInt64 {
    owner.assertCurrent()
    guard let add = library.addDanmakuTrackJson else {
      throw ErikaPluginError.symbolMissing("erika_presenter_add_danmaku_track_json")
    }
    var trackId: UInt64 = 0
    let status = json.withCString { jsonCString in
      withOptionalCString(name) { nameCString in
        add(handle, jsonCString, nameCString, offsetMicros, &trackId)
      }
    }
    try check(status, operation: "add_danmaku_track_json")
    return trackId
  }

  func removeDanmakuTrack(trackId: UInt64) throws {
    owner.assertCurrent()
    guard let remove = library.removeDanmakuTrack else {
      throw ErikaPluginError.symbolMissing("erika_presenter_remove_danmaku_track")
    }
    try check(remove(handle, trackId), operation: "remove_danmaku_track")
  }

  func setDanmakuTrackEnabled(trackId: UInt64, enabled: Bool) throws {
    owner.assertCurrent()
    guard let setEnabled = library.setDanmakuTrackEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_track_enabled")
    }
    try check(setEnabled(handle, trackId, enabled), operation: "set_danmaku_track_enabled")
  }

  func setDanmakuTrackOffset(trackId: UInt64, offsetMicros: Int64) throws {
    owner.assertCurrent()
    guard let setOffset = library.setDanmakuTrackOffset else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_track_offset")
    }
    try check(setOffset(handle, trackId, offsetMicros), operation: "set_danmaku_track_offset")
  }

  func setDanmakuGlobalOffset(offsetMicros: Int64) throws {
    owner.assertCurrent()
    guard let setOffset = library.setDanmakuGlobalOffset else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_global_offset")
    }
    try check(setOffset(handle, offsetMicros), operation: "set_danmaku_global_offset")
  }

  func danmakuTracks() throws -> [[String: Any]] {
    owner.assertCurrent()
    guard let danmakuTracks = library.danmakuTracks else {
      throw ErikaPluginError.symbolMissing("erika_presenter_danmaku_tracks")
    }
    var count: Int = 0
    try check(danmakuTracks(handle, nil, 0, &count), operation: "danmaku_tracks_len")
    if count <= 0 { return [] }
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

  func clearDanmaku() throws {
    owner.assertCurrent()
    guard let clear = library.clearDanmaku else {
      throw ErikaPluginError.symbolMissing("erika_presenter_clear_danmaku")
    }
    try check(clear(handle), operation: "clear_danmaku")
  }

  func setDanmakuEnabled(_ enabled: Bool) throws {
    owner.assertCurrent()
    guard let setEnabled = library.setDanmakuEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_enabled")
    }
    try check(setEnabled(handle, enabled), operation: "set_danmaku_enabled")
    currentDanmakuConfig.enabled = enabled ? 1 : 0
  }

  func setDebugHudEnabled(_ enabled: Bool) throws {
    owner.assertCurrent()
    guard let setEnabled = library.setDebugHudEnabled else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_debug_hud_enabled")
    }
    try check(setEnabled(handle, enabled), operation: "set_debug_hud_enabled")
  }

  func danmakuConfigSnapshot() -> ErikaDanmakuConfigC {
    currentDanmakuConfig
  }

  private func refreshDanmakuConfigSnapshot() {
    owner.assertCurrent()
    guard let getConfig = library.getDanmakuConfig else {
      return
    }
    var config = ErikaDanmakuConfigC()
    let status = withUnsafeMutablePointer(to: &config) { pointer in
      getConfig(handle, UnsafeMutableRawPointer(pointer))
    }
    if status == 0 {
      currentDanmakuConfig = config
    }
  }

  func setDanmakuConfig(_ config: ErikaDanmakuConfigC) throws {
    owner.assertCurrent()
    guard let setConfig = library.setDanmakuConfig else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_config_ptr")
    }
    var config = config
    let status = withUnsafePointer(to: &config) { pointer in
      setConfig(handle, UnsafeRawPointer(pointer))
    }
    try check(status, operation: "set_danmaku_config")
    currentDanmakuConfig = config
  }

  func setDanmakuFont(family: String?, filePath: String?) throws {
    owner.assertCurrent()
    guard let setFont = library.setDanmakuFont else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_font")
    }
    let status = withOptionalCString(family ?? "") { familyCString in
      withOptionalCString(filePath ?? "") { filePathCString in
        setFont(handle, familyCString, filePathCString)
      }
    }
    try check(status, operation: "set_danmaku_font")
    refreshDanmakuConfigSnapshot()
  }

  func setDanmakuBlockWordsJson(_ json: String) throws {
    owner.assertCurrent()
    guard let setBlockWords = library.setDanmakuBlockWords else {
      throw ErikaPluginError.symbolMissing("erika_presenter_set_danmaku_block_words_json")
    }
    try json.withCString { cString in
      try check(setBlockWords(handle, cString), operation: "set_danmaku_block_words")
    }
    refreshDanmakuConfigSnapshot()
  }

  func selectAudioTrack(trackId: Int64?) throws {
    owner.assertCurrent()
    try check(library.selectAudioTrack(handle, trackId ?? -1), operation: "select_audio_track")
  }

  func selectSubtitleTrack(trackId: Int64?) throws {
    owner.assertCurrent()
    try check(library.selectSubtitleTrack(handle, trackId ?? -1), operation: "select_subtitle_track")
  }

  func tracks() throws -> [[String: Any]] {
    owner.assertCurrent()
    var count: Int = 0
    try check(library.tracks(handle, nil, 0, &count), operation: "tracks_len")
    if count <= 0 { return [] }
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

  func trackSelection() throws -> [String: Any] {
    owner.assertCurrent()
    var selection = ErikaTrackSelectionC()
    let status = withUnsafeMutablePointer(to: &selection) { pointer in
      library.trackSelection(handle, UnsafeMutableRawPointer(pointer))
    }
    try check(status, operation: "track_selection")
    return selection.toFlutterMap()
  }

  func captureFrameRgba(width: Int, height: Int) -> Data? {
    owner.assertCurrent()
    guard width > 0, height > 0, width <= 16384, height <= 16384, let captureFrameRgba = library.captureFrameRgba else {
      return nil
    }
    let byteCount = width * height * 4
    var data = Data(count: byteCount)
    let status = data.withUnsafeMutableBytes { buffer in
      captureFrameRgba(
        handle,
        UInt32(width),
        UInt32(height),
        buffer.baseAddress,
        byteCount
      )
    }
    guard status == 0 else {
      NSLog("Erika: captureFrameRgba failed with status \(status)")
      return nil
    }
    return data
  }

  func snapshotView() -> ErikaMetalSurfaceView? {
    precondition(Thread.isMainThread)
    return attachedView
  }

  func screenshot(view: ErikaMetalSurfaceView? = nil, width: Int? = nil, height: Int? = nil) -> Data? {
    owner.assertCurrent()
    if let width, let height { return captureFrameRgba(width: width, height: height) }
    return nil
  }

  func attach(view: ErikaMetalSurfaceView) throws {
    attachedView?.attachedPlayerId = nil
    attachedView = view
    attachedViewId = view.platformViewId
    view.attachedPlayerId = id
    try attachOrResize(view: view, attach: true)
    view.attachedPlayerId = id // a transfer detached the previous owner
    startDisplayLinkIfNeeded()
    updateTickDriver()
  }

  func detach(viewId: Int64?) {
    guard viewId == nil || attachedViewId == viewId else { return }
    attachedView?.attachedPlayerId = nil
    attachedView = nil
    attachedViewId = nil
    surfaceLease = nil
    displayLink?.invalidate()
    displayLink = nil
    displayLinkProxy = nil
    owner.cancelFrame()
    enqueueDetach()
    updateTickDriver()
  }

  func resizeFromAttachedView() {
    guard let view = attachedView else { return }
    do {
      try attachOrResize(view: view, attach: false)
    } catch {
      NSLog("ErikaFlutterPlugin: resize failed: \(error)")
    }
  }

  func renderTick(presentationTime: Double? = nil) {
    if !loggedRenderThread {
      loggedRenderThread = true
      erikaHdrLog(
        hdrDebug,
        "render driver player=\(id) mainThread=\(Thread.isMainThread) displayTarget=\(library.renderTickWithTiming != nil)"
      )
    }
    var stats = ErikaPresenterStatsC()
    var skippedTick = false
    let status = withNativeCall {
      let now = CACurrentMediaTime()
      let timeSeconds = (presentationTime ?? now) - startTimeSeconds
      let status = withUnsafeMutablePointer(to: &stats) { pointer in
        if let target = presentationTime, let renderAt = library.renderTickWithTiming {
          var delay = target - CACurrentMediaTime()
          // Reject an obsolete display target after a slow command. The
          // owner mailbox keeps the newest pending target.
          guard abs(delay) <= 0.25 else {
            skippedTick = true
            return Int32(0)
          }
          return withUnsafePointer(to: &delay) { delayPointer in
            renderAt(handle, timeSeconds, delayPointer, UnsafeMutableRawPointer(pointer))
          }
        }
        return library.renderTick(handle, timeSeconds, UnsafeMutableRawPointer(pointer))
      }
      if status == 0 && !skippedTick {
        latestPresenterStats = stats
      }
      return status
    }
    if skippedTick { return }
    if status != 0 {
      NSLog("ErikaFlutterPlugin: render_tick failed with status \(status)")
    }
    if hdrDebug && stats.renderedVideoFrames > 0 {
      let statsSnapshot = stats
      DispatchQueue.main.async { [weak self] in
        self?.logFirstRenderedVideoFrameIfNeeded(statsSnapshot)
      }
    }
  }

  func audioOnlyTick() {
    var stats = ErikaPresenterStatsC()
    let status = withNativeCall {
      let status = withUnsafeMutablePointer(to: &stats) { pointer in
        library.audioOnlyTick(handle, UnsafeMutableRawPointer(pointer))
      }
      if status == 0 {
        latestPresenterStats = stats
      }
      return status
    }
    if status != 0 {
      NSLog("ErikaFlutterPlugin: audio_only_tick failed with status \(status)")
    }
  }

  func pollEvents(sendEvent: (([String: Any]) -> Void)?) {
    if !owner.isCurrent {
      owner.requestMaintenance { [weak self] in
        guard let self, self.handle != nil else { return }
        self.pollEvents(sendEvent: sendEvent)
      }
      return
    }

    // Coalesced maintenance requests execute on the native owner.
    withNativeCall {
      for _ in 0..<256 {
        var event = ErikaEventC()
        let status = withUnsafeMutablePointer(to: &event) { pointer in
          library.pollEvent(handle, UnsafeMutableRawPointer(pointer))
        }
        if status == 0 {
          if event.durationMicros >= 0 {
            durationSeconds = Double(event.durationMicros) / 1_000_000
          }
          if event.kind == 3 {
            positionSeconds = Double(event.positionMicros) / 1_000_000
          }
          if event.kind == 1 {
            isPlaying = event.state == 3
            updateTickDriver()
          }
          if event.kind == 1 || event.kind == 2 || event.kind == 3 {
            notifyNowPlayingChanged()
          }
          if event.kind == 6 {
            erikaHdrLog(
              hdrDebug,
              "video params player=\(id) width=\(event.video.width) height=\(event.video.height) primaries=\(event.video.primaries) transfer=\(event.video.transfer)"
            )
          }
          let message = event.kind == 9 || event.kind == 11 || event.kind == 12
            ? library.currentEventMessage()
            : nil
          let map = event.toFlutterMap(playerId: id, host: self, structuredMessage: message)
          DispatchQueue.main.async { [weak self] in
            guard let self, !self.closed else { return }
            sendEvent?(map)
          }
          continue
        }
        if status != 5 {
          NSLog("ErikaFlutterPlugin: poll_event failed with status \(status)")
        }
        break
      }
    }
  }

  private func scheduleTick(audioOnly: Bool = false, presentationTime: Double? = nil) {
    owner.requestFrame { [weak self] in
      guard let self, self.handle != nil else { return }
      if audioOnly { self.audioOnlyTick() }
      else { self.renderTick(presentationTime: presentationTime) }
    }
  }

  private func logFirstRenderedVideoFrameIfNeeded(_ stats: ErikaPresenterStatsC) {
    guard hdrDebug, !loggedFirstRenderedVideoFrame else { return }
    loggedFirstRenderedVideoFrame = true
    let layer = attachedView.map { erikaLayerSummary($0.metalLayer) } ?? "layer=nil"
    let screen = erikaScreenSummary(attachedView?.window?.screen ?? UIScreen.main)
    erikaHdrLog(
      true,
      "first rendered frame player=\(id) mode=\(erikaOutputModeLabel(presenterConfig)) decoded=\(stats.decodedVideoFrames) rendered=\(stats.renderedVideoFrames) test=\(stats.renderedTestFrames) \(screen) \(layer)"
    )
  }

  private func attachOrResize(view: ErikaMetalSurfaceView, attach: Bool) throws {
    precondition(Thread.isMainThread)
    if attach || presenterConfig.outputMode != 3 {
      erikaConfigureLayerDynamicRange(view.metalLayer, config: presenterConfig)
    }
    view.updateDrawableSize()
    let layer = view.metalLayer
    let width = UInt32(max(1.0, layer.drawableSize.width).rounded())
    let height = UInt32(max(1.0, layer.drawableSize.height).rounded())
    let scale = view.currentScale
    let claim = attach ? claimSurface(previous: layer.erikaOwnerLease) : nil
    if let claim { layer.erikaOwnerLease = claim.lease }
    owner.post { [self, layer] in
      claim?.dependency?.wait()
      guard handle != nil else { claim?.lease.complete(); return }
      let previousLease = nativeLease
      let previousLayer = nativeLayer
      defer {
        DispatchQueue.main.async {
          withExtendedLifetime(previousLayer) {}
        }
      }
      do {
        if attach {
          surfaceError = nil
          let rawLayer = UInt64(UInt(bitPattern: Unmanaged.passUnretained(layer).toOpaque()))
          try check(library.attachMetalLayer(handle, rawLayer, width, height, scale), operation: "attach_metal_layer")
          nativeLayer = layer
          nativeLease = claim?.lease
          previousLease?.complete()
        } else {
          try check(library.resizeSurface(handle, width, height, scale), operation: "resize_surface")
        }
      } catch {
        // A failed attach cannot leave a transfer waiting for a future UI
        // callback. Detach any partial native surface before releasing claims.
        if attach {
          detachNativeSurface()
          nativeLease?.complete()
          nativeLease = nil
          claim?.lease.complete()
        }
        surfaceError = error; NSLog("Erika: surface update failed: \(error)") }
    }
  }

  private func startDisplayLinkIfNeeded() {
    guard displayLink == nil, attachedView != nil, !isAppInBackground else { return }
    startTimeSeconds = CACurrentMediaTime()
    let proxy = DisplayLinkProxy { [weak self] targetTimestamp in
      self?.scheduleTick(presentationTime: targetTimestamp)
    }
    let link = CADisplayLink(target: proxy, selector: #selector(DisplayLinkProxy.tick(_:)))
    let fps = resolvedDisplayLinkFps()
    if #available(iOS 15.0, *) {
      let maximum = Float(attachedView?.window?.screen.maximumFramesPerSecond ?? UIScreen.main.maximumFramesPerSecond)
      let preferred = min(Float(fps), maximum)
      link.preferredFrameRateRange = CAFrameRateRange(minimum: min(60, preferred), maximum: preferred, preferred: preferred)
    } else {
      link.preferredFramesPerSecond = fps
    }
    link.add(to: .main, forMode: .common)
    displayLinkProxy = proxy
    displayLink = link
  }

  private func updateTickDriver() {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in self?.updateTickDriver() }
      return
    }
    guard !closed else { return }
    if isAppInBackground || (attachedView == nil && isPlaying) {
      displayLink?.invalidate()
      displayLink = nil
      displayLinkProxy = nil
      owner.cancelFrame()
      startFallbackTimerIfNeeded()
    } else {
      if fallbackTimer != nil { owner.cancelFrame() }
      fallbackTimer?.cancel()
      fallbackTimer = nil
      startDisplayLinkIfNeeded()
    }
  }

  private func startFallbackTimerIfNeeded() {
    guard fallbackTimer == nil, isPlaying else { return }
    let timer = DispatchSource.makeTimerSource(queue: .main)
    timer.schedule(deadline: .now(), repeating: .milliseconds(16), leeway: .milliseconds(2))
    timer.setEventHandler { [weak self] in
      guard let self else { return }
      if self.isAppInBackground {
        self.scheduleTick(audioOnly: true)
      } else {
        self.scheduleTick()
      }
    }
    fallbackTimer = timer
    timer.resume()
  }

  private func notifyNowPlayingChanged() {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in self?.notifyNowPlayingChanged() }
      return
    }
    guard !closed else { return }
    onNowPlayingChanged?(self)
  }

  private func resolvedDisplayLinkFps() -> Int {
    if let override = ProcessInfo.processInfo.environment["ERIKA_FLUTTER_TARGET_FPS"],
       let fps = Int(override), fps > 0 {
      return min(max(fps, 1), 1000)
    }
    let fps = attachedView?.window?.screen.maximumFramesPerSecond ?? UIScreen.main.maximumFramesPerSecond
    return fps > 0 ? fps : erikaDefaultDisplayFps
  }

  private func check(_ status: Int32, operation: String) throws {
    if status != 0 {
      throw ErikaPluginError.erikaStatus(operation, status)
    }
  }

  func prepareAudioSession() throws {
    precondition(Thread.isMainThread)
    let session = AVAudioSession.sharedInstance()
    try session.setCategory(.playback, mode: .moviePlayback, options: [])
    try session.setActive(true)
  }
}

private final class DisplayLinkProxy: NSObject {
  private let body: (Double) -> Void

  init(_ body: @escaping (Double) -> Void) {
    self.body = body
  }

  @objc func tick(_ link: CADisplayLink) {
    body(link.targetTimestamp)
  }
}

private protocol ErikaMetalSurfaceView: AnyObject {
  var platformViewId: Int64 { get }
  var metalLayer: CAMetalLayer { get }
  var attachedPlayerId: Int64? { get set }
  var bounds: CGRect { get }
  var window: UIWindow? { get }
  var currentScale: Double { get }

  func updateDrawableSize()
  func pngSnapshotData() -> Data?
}

private final class WeakErikaVideoPlatformViewBox {
  weak var view: ErikaMetalSurfaceView?

  init(view: ErikaMetalSurfaceView) {
    self.view = view
  }
}

private final class ErikaMetalUIView: UIView, ErikaMetalSurfaceView {
  let platformViewId: Int64
  weak var plugin: ErikaFlutterPlugin?
  var attachedPlayerId: Int64?

  override class var layerClass: AnyClass { CAMetalLayer.self }

  var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

  var currentScale: Double {
    Double(max(1.0, window?.screen.scale ?? UIScreen.main.scale))
  }

  init(frame: CGRect, viewId: Int64, arguments: Any?, plugin: ErikaFlutterPlugin?) {
    platformViewId = viewId
    self.plugin = plugin
    super.init(frame: frame)
    isOpaque = true
    isUserInteractionEnabled = false
    backgroundColor = .black
    contentScaleFactor = CGFloat(currentScale)
    metalLayer.pixelFormat = .bgra8Unorm
    metalLayer.framebufferOnly = true
    metalLayer.isOpaque = true
    metalLayer.backgroundColor = UIColor.black.cgColor
  }

  required init?(coder: NSCoder) {
    fatalError("init(coder:) has not been implemented")
  }

  deinit {
    plugin?.unregisterView(viewId: platformViewId)
  }

  override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
    false
  }

  override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
    nil
  }

  override func layoutSubviews() {
    super.layoutSubviews()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func didMoveToWindow() {
    super.didMoveToWindow()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  func updateDrawableSize() {
    let scale = CGFloat(currentScale)
    contentScaleFactor = scale
    metalLayer.contentsScale = scale
    // metalLayer is this view's *backing* layer (layerClass == CAMetalLayer);
    // UIKit already syncs its frame to the view. Setting it to `bounds` would
    // move the backing layer to the superlayer origin, rendering the video at
    // (0,0) instead of the view's frame (visible once the view isn't full-screen).
    metalLayer.drawableSize = CGSize(
      width: max(1.0, bounds.width * scale),
      height: max(1.0, bounds.height * scale)
    )
  }

  func pngSnapshotData() -> Data? {
    snapshotPngData(of: self)
  }
}

private final class ErikaWindowOverlayView: UIView, ErikaMetalSurfaceView {
  let platformViewId: Int64 = erikaWindowHostedVideoSurfaceId
  weak var plugin: ErikaFlutterPlugin?
  var attachedPlayerId: Int64?

  private var overlayFrameGeneration: Int64?
  private var debugLabelView: UILabel?

  /// Generation of the widget that currently owns this shared overlay surface.
  /// Used to reject stale detach calls from disposed widgets.
  var activeGeneration: Int64? { overlayFrameGeneration }

  override class var layerClass: AnyClass { CAMetalLayer.self }

  var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

  var currentScale: Double {
    Double(max(1.0, window?.screen.scale ?? UIScreen.main.scale))
  }

  init(plugin: ErikaFlutterPlugin?) {
    self.plugin = plugin
    super.init(frame: .zero)
    isOpaque = true
    isHidden = true
    isUserInteractionEnabled = false
    backgroundColor = .black
    contentScaleFactor = CGFloat(currentScale)
    autoresizingMask = []
    metalLayer.pixelFormat = .bgra8Unorm
    metalLayer.framebufferOnly = true
    metalLayer.isOpaque = true
    metalLayer.backgroundColor = UIColor.black.cgColor
    layer.actions = [
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

  override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
    false
  }

  override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
    nil
  }

  override func layoutSubviews() {
    super.layoutSubviews()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  override func didMoveToWindow() {
    super.didMoveToWindow()
    updateDrawableSize()
    plugin?.resizePlayerAttachedToView(viewId: platformViewId)
  }

  func updateOverlayFrame(
    _ frame: CGRect?,
    visible: Bool,
    debugLabel: String?,
    generation: Int64?
  ) {
    if visible {
      overlayFrameGeneration = generation
    } else if let generation,
              let overlayFrameGeneration,
              generation != overlayFrameGeneration {
      return
    }

    updateDebugLabel(debugLabel)
    let shouldShow = visible &&
      (frame?.width ?? 0) > 0 &&
      (frame?.height ?? 0) > 0

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
    let scale = CGFloat(currentScale)
    contentScaleFactor = scale
    metalLayer.contentsScale = scale
    // metalLayer is this view's *backing* layer (layerClass == CAMetalLayer);
    // UIKit already syncs its frame to the view. Setting it to `bounds` would
    // move the backing layer to the superlayer origin, rendering the video at
    // (0,0) instead of the view's frame (visible once the view isn't full-screen).
    metalLayer.drawableSize = CGSize(
      width: max(1.0, bounds.width * scale),
      height: max(1.0, bounds.height * scale)
    )
  }

  func pngSnapshotData() -> Data? {
    snapshotPngData(of: self)
  }

  private func updateDebugLabel(_ text: String?) {
    guard erikaDebugLabelsEnabled, let text, !text.isEmpty else {
      debugLabelView?.removeFromSuperview()
      debugLabelView = nil
      return
    }
    let label = debugLabelView ?? UILabel()
    if debugLabelView == nil {
      label.textColor = UIColor(white: 1.0, alpha: 0.45)
      label.font = UIFont.systemFont(ofSize: 12, weight: .medium)
      label.translatesAutoresizingMaskIntoConstraints = false
      addSubview(label)
      NSLayoutConstraint.activate([
        label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
        label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -10),
      ])
      debugLabelView = label
    }
    label.text = text
  }
}

private func snapshotPngData(of view: UIView) -> Data? {
  guard view.bounds.width > 0, view.bounds.height > 0 else {
    return nil
  }
  let format = UIGraphicsImageRendererFormat()
  format.scale = view.window?.screen.scale ?? UIScreen.main.scale
  format.opaque = view.isOpaque
  let renderer = UIGraphicsImageRenderer(bounds: view.bounds, format: format)
  let image = renderer.image { _ in
    view.drawHierarchy(in: view.bounds, afterScreenUpdates: false)
  }
  return image.pngData()
}

private final class ErikaVideoPlatformView: NSObject, FlutterPlatformView {
  let metalView: ErikaMetalUIView

  init(frame: CGRect, viewId: Int64, arguments: Any?, plugin: ErikaFlutterPlugin?) {
    metalView = ErikaMetalUIView(frame: frame, viewId: viewId, arguments: arguments, plugin: plugin)
    super.init()
  }

  func view() -> UIView {
    metalView
  }
}

private final class ErikaVideoViewFactory: NSObject, FlutterPlatformViewFactory {
  private weak var plugin: ErikaFlutterPlugin?

  init(plugin: ErikaFlutterPlugin) {
    self.plugin = plugin
    super.init()
  }

  func createArgsCodec() -> FlutterMessageCodec & NSObjectProtocol {
    FlutterStandardMessageCodec.sharedInstance()
  }

  func create(
    withFrame frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?
  ) -> FlutterPlatformView {
    let platformView = ErikaVideoPlatformView(frame: frame, viewId: viewId, arguments: args, plugin: plugin)
    plugin?.registerView(platformView.metalView, viewId: viewId)
    return platformView
  }
}

private enum ErikaAssociatedObjectKeys {
  static var windowOverlayView: UInt8 = 0
}

private extension UIWindow {
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

public final class ErikaFlutterPlugin: NSObject, FlutterPlugin, FlutterStreamHandler {
  private var sharedEventSink: FlutterEventSink?
  private var subscription = 0

  private static let playerChannelName = "erika_flutter/player"
  private static let eventsChannelName = "erika_flutter/events"
  private static let videoViewType = "erika_flutter/video_view"

  private let delivery = ErikaPlatformDelivery()
  private var players: [Int64: ErikaPlayerHost] = [:]
  private var views: [Int64: WeakErikaVideoPlatformViewBox] = [:]
  private var nextPlayerId: Int64 = 1
  private let logQueue = DispatchQueue(label: "dev.aimesoft.erika.logs", qos: .utility)
  private var pollTimer: Timer?
  private var activePlayerId: Int64?
  private var interruptedPlayerId: Int64?
  private var interruptionResumeWorkItem: DispatchWorkItem?
  private var notificationObservers: [NSObjectProtocol] = []
  private var remoteCommandTargets: [(MPRemoteCommand, Any)] = []
  private var systemMediaNavigation: [Int64: (previousEnabled: Bool, nextEnabled: Bool)] = [:]

  deinit {
    delivery.invalidate()
    sharedEventSink = nil
    pollTimer?.invalidate()
    players.values.forEach { $0.shutdown() }
    interruptionResumeWorkItem?.cancel()
    notificationObservers.forEach(NotificationCenter.default.removeObserver)
    remoteCommandTargets.forEach { command, target in
      command.removeTarget(target)
    }
  }

  public static func register(with registrar: FlutterPluginRegistrar) {
    let instance = ErikaFlutterPlugin()
    instance.configureSystemPlayback()
    registrar.publish(instance)
    let playerChannel = FlutterMethodChannel(name: playerChannelName, binaryMessenger: registrar.messenger())
    let eventsChannel = FlutterEventChannel(name: eventsChannelName, binaryMessenger: registrar.messenger())
    registrar.addMethodCallDelegate(instance, channel: playerChannel)
    eventsChannel.setStreamHandler(instance)
    registrar.register(ErikaVideoViewFactory(plugin: instance), withId: videoViewType)
  }

  public func detachFromEngine(for registrar: FlutterPluginRegistrar) {
    delivery.invalidate()
    sharedEventSink = nil
    pollTimer?.invalidate()
    players.values.forEach { $0.shutdown() }
    players.removeAll()
    activePlayerId = nil
    clearNowPlayingInfo()
    remoteCommandTargets.forEach { command, target in
      command.isEnabled = false
      command.removeTarget(target)
    }
    remoteCommandTargets.removeAll()
    interruptionResumeWorkItem?.cancel()
    notificationObservers.forEach(NotificationCenter.default.removeObserver)
    notificationObservers.removeAll()
  }

  #if DEBUG
  func enterBackgroundForTesting(playerId: Int64) {
    players[playerId]?.didEnterBackground()
  }

  func makeSurfaceForTesting(viewId: Int64) -> AnyObject {
    let view = ErikaMetalUIView(frame: .zero, viewId: viewId, arguments: nil, plugin: self)
    registerView(view, viewId: viewId)
    return view
  }

  func requestFrameForTesting(playerId: Int64) {
    guard let host = players[playerId] else { return }
    host.owner.requestFrame { [weak host] in
      guard let host else { return }
      host.renderTick()
    }
  }

  func shutdownForTesting() {
    delivery.invalidate()
    sharedEventSink = nil
    pollTimer?.invalidate()
    players.values.forEach { $0.shutdown() }
    players.removeAll()
  }
  #endif

  public func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    let result = delivery.reserve(
      key: int64Value((call.arguments as? [String: Any])?["playerId"]) ?? 0, result)
    do {
      switch call.method {
      case "create":
        let id = try createPlayer(arguments: call.arguments)
        let host = players[id]!
        host.created { [weak self] value in
          if value is FlutterError, let self, self.players[id] === host {
            self.players.removeValue(forKey: id)
            host.shutdown()
          }
          result(value)
        }
      case "dispose":
        let args = try dictionaryArgs(call.arguments)
        let playerId = try requiredInt64(args["playerId"], name: "playerId")
        let retiringHost = players.removeValue(forKey: playerId)
        systemMediaNavigation.removeValue(forKey: playerId)
        cancelPendingInterruptionResume(ifPlayer: playerId)
        if activePlayerId == playerId {
          activePlayerId = nil
          clearNowPlayingInfo()
          refreshRemoteCommands()
        }
        if let retiringHost { retiringHost.shutdown { result(nil) } } else { result(nil) }
      case "open":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
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
        operations.append {
          [
            host, argument1 = uri, argument2 = headers, argument3 = readAhead,
            argument4 = backBuffer
          ] in
          try host.open(
            uri: argument1, httpHeaders: argument2, httpReadAheadBytes: argument3,
            httpBackBufferBytes: argument4);
          return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "play":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        try host.prepareAudioSession()
        operations.append { [host] in
          try host.play(); return nil
        }
        let completion: FlutterResult = { [weak self] value in
          if !(value is FlutterError), let self, self.players[host.id] === host {
            self.activePlayerId = host.id
            self.refreshRemoteCommands()
            self.updateNowPlayingInfo(for: host)
          }
          result(value)
        }
        host.enqueue(operations: operations, result: completion, label: call.method)
      case "pause":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        // Drop a pending interruption resume first: it is scheduled on the main
        // queue and would otherwise fire after this pause and play again.
        cancelPendingInterruptionResume(ifPlayer: host.id)
        operations.append { [host] in
          try host.pause(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "stop":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        cancelPendingInterruptionResume(ifPlayer: host.id)
        operations.append { [host] in
          try host.stop(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "close":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        cancelPendingInterruptionResume(ifPlayer: host.id)
        operations.append { [host] in
          try host.close(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "seek":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append {
          [host, argument1 = try requiredUInt64(args["positionMicros"], name: "positionMicros")] in
          try host.seek(positionMicros: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setPlaybackRate":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let rate = doubleValue(args["rate"]) else {
          throw ErikaPluginError.invalidArguments("rate is required.")
        }
        operations.append { [host, argument1 = rate] in
          try host.setPlaybackRate(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
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
        }
        result(nil)
      case "setVolume":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let volume = doubleValue(args["volume"]) else {
          throw ErikaPluginError.invalidArguments("volume is required.")
        }
        operations.append { [host, argument1 = volume] in
          try host.setVolume(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setUpscaler":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let mode = int32Value(args["mode"]) else {
          throw ErikaPluginError.invalidArguments("mode is required.")
        }
        operations.append { [host, argument1 = mode] in
          try host.setUpscaler(mode: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "loadUpscalerModel":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let data = args["data"] as? FlutterStandardTypedData, !data.data.isEmpty else {
          throw ErikaPluginError.invalidArguments("data is required.")
        }
        operations.append {
          [host, argument1 = args["name"] as? String ?? "custom", argument2 = data.data] in
          try host.loadUpscalerModel(name: argument1, data: argument2); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "clearUpscalerModel":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          try host.clearUpscalerModel(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setSubtitleScale":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let scale = doubleValue(args["scale"]) else {
          throw ErikaPluginError.invalidArguments("scale is required.")
        }
        operations.append { [host, argument1 = scale] in
          try host.setSubtitleScale(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "registerSubtitleMemoryFont":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let data = args["data"] as? FlutterStandardTypedData else {
          throw ErikaPluginError.invalidArguments("data is required.")
        }
        operations.append { [host, argument1 = data.data] in
          return Int64(clamping: try host.registerSubtitleMemoryFont(argument1))
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "selectSubtitleMemoryFonts":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        let ids = (args["fontIds"] as? [NSNumber] ?? []).map { $0.uint64Value }
        operations.append { [host, argument1 = ids] in
          try host.selectSubtitleMemoryFonts(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "clearSubtitleMemoryFonts":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          try host.clearSubtitleMemoryFonts(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "getSubtitleMemoryFontStatus":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.subtitleMemoryFontStatus()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setSubtitleStyle":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        if args.keys.contains("fontFamily") || args.keys.contains("fontFilePath") {
          operations.append {
            [
              host, argument1 = args["fontFamily"] as? String,
              argument2 = args["fontFilePath"] as? String
            ] in
            try host.setSubtitleFont(family: argument1, filePath: argument2); return nil
          }
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
          operations.append {
            [
              host, argument1 = args["fontFamily"] as? String,
              argument2 = args["fontFilePath"] as? String,
              argument3 = UInt32(truncatingIfNeeded: primary),
              argument4 = UInt32(truncatingIfNeeded: outline),
              argument5 = doubleValue(args["fontSize"]) ?? 48.0,
              argument6 = doubleValue(args["outlineWidth"]) ?? 2.0,
              argument7 = boolValue(args["bold"]) ?? false,
              argument8 = boolValue(args["italic"]) ?? false,
              argument9 = boolValue(args["underline"]) ?? false,
              argument10 = boolValue(args["strikeOut"]) ?? false,
              argument11 = doubleValue(args["spacing"]) ?? 0.0,
              argument12 = doubleValue(args["scaleXPercent"]) ?? 100.0,
              argument13 = doubleValue(args["scaleYPercent"]) ?? 100.0,
              argument14 = int32Value(args["borderStyle"]) ?? 1,
              argument15 = doubleValue(args["shadowDepth"]) ?? 0.0,
              argument16 = doubleValue(args["blur"]) ?? 0.0,
              argument17 = int32Value(args["alignment"]) ?? 2,
              argument18 = int32Value(args["marginLeft"]) ?? 48,
              argument19 = int32Value(args["marginRight"]) ?? 48,
              argument20 = int32Value(args["marginVertical"]) ?? 54,
              argument21 = UInt32(truncatingIfNeeded: int64Value(args["overrideMask"]) ?? 0)
            ] in
            try host.setSubtitleStyle(
              fontFamily: argument1, fontFilePath: argument2, primaryRgba: argument3,
              outlineRgba: argument4, fontSize: argument5, outlineWidth: argument6, bold: argument7,
              italic: argument8, underline: argument9, strikeOut: argument10, spacing: argument11,
              scaleXPercent: argument12, scaleYPercent: argument13, borderStyle: argument14,
              shadowDepth: argument15, blur: argument16, alignment: argument17,
              marginLeft: argument18, marginRight: argument19, marginVertical: argument20,
              overrideMask: argument21);
            return nil
          }
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "getUpscalerStatus":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.upscalerStatus()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "getOutputStatus":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.outputStatus()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "getResourceStatus":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.resourceStatus()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "getDiagnosticsSnapshot":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.diagnosticsSnapshotJson()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "invokeJson":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
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
        operations.append { [host, argument1 = method, argument2 = argumentsJson] in
          return try host.invokeJson(method: argument1, argumentsJson: argument2)
        }
        host.enqueue(operations: operations, result: result, label: call.method)
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
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return host.presenterStats()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDebugHudEnabled":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, argument1 = boolValue(args["enabled"]) ?? false] in
          try host.setDebugHudEnabled(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "addExternalSubtitle":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        operations.append { [host, argument1 = uri] in
          return try host.addExternalSubtitle(uri: argument1)
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "removeSubtitleTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append {
          [host, argument1 = try requiredInt64(args["trackId"], name: "trackId")] in
          try host.removeSubtitleTrack(trackId: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "loadDanmakuFile":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        operations.append { [host, argument1 = uri] in
          try host.loadDanmakuFile(uri: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "loadDanmakuJson":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let json = args["json"] as? String, !json.isEmpty else {
          throw ErikaPluginError.invalidArguments("json is required.")
        }
        operations.append { [host, argument1 = json] in
          try host.loadDanmakuJson(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "addDanmakuTrackFile":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let uri = args["uri"] as? String, !uri.isEmpty else {
          throw ErikaPluginError.invalidArguments("uri is required.")
        }
        operations.append {
          [
            host, argument1 = uri, argument2 = args["name"] as? String,
            argument3 = int64Value(args["offsetMicros"]) ?? 0
          ] in
          return Int64(
            clamping: try host.addDanmakuTrackFile(
              uri: argument1, name: argument2, offsetMicros: argument3))
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "addDanmakuTrackJson":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        guard let json = args["json"] as? String, !json.isEmpty else {
          throw ErikaPluginError.invalidArguments("json is required.")
        }
        operations.append {
          [
            host, argument1 = json, argument2 = args["name"] as? String,
            argument3 = int64Value(args["offsetMicros"]) ?? 0
          ] in
          return Int64(
            clamping: try host.addDanmakuTrackJson(
              argument1, name: argument2, offsetMicros: argument3))
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "removeDanmakuTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append {
          [host, argument1 = try requiredUInt64(args["trackId"], name: "trackId")] in
          try host.removeDanmakuTrack(trackId: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDanmakuTrackEnabled":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append {
          [
            host, argument1 = try requiredUInt64(args["trackId"], name: "trackId"),
            argument2 = boolValue(args["enabled"]) ?? true
          ] in
          try host.setDanmakuTrackEnabled(trackId: argument1, enabled: argument2); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDanmakuTrackOffset":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append {
          [
            host, argument1 = try requiredUInt64(args["trackId"], name: "trackId"),
            argument2 = int64Value(args["offsetMicros"]) ?? 0
          ] in
          try host.setDanmakuTrackOffset(trackId: argument1, offsetMicros: argument2); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDanmakuGlobalOffset":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, argument1 = int64Value(args["offsetMicros"]) ?? 0] in
          try host.setDanmakuGlobalOffset(offsetMicros: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "danmakuTracks":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.danmakuTracks()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "clearDanmaku":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          try host.clearDanmaku(); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDanmakuEnabled":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, argument1 = boolValue(args["enabled"]) ?? true] in
          try host.setDanmakuEnabled(argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "setDanmakuConfig":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, patch = danmakuConfigPatch(from: args)] in
          try host.setDanmakuConfig(patch(host.danmakuConfigSnapshot())); return nil
        }
        if args.keys.contains("customFontFamily") || args.keys.contains("customFontFilePath") {
          operations.append {
            [
              host, argument1 = args["customFontFamily"] as? String,
              argument2 = args["customFontFilePath"] as? String
            ] in
            try host.setDanmakuFont(family: argument1, filePath: argument2); return nil
          }
        }
        if let blockWordsJson = args["blockWordsJson"] as? String {
          operations.append { [host, argument1 = blockWordsJson] in
            try host.setDanmakuBlockWordsJson(argument1); return nil
          }
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "selectAudioTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, argument1 = try optionalTrackId(args["trackId"])] in
          try host.selectAudioTrack(trackId: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "selectSubtitleTrack":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host, argument1 = try optionalTrackId(args["trackId"])] in
          try host.selectSubtitleTrack(trackId: argument1); return nil
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "tracks":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        var operations: [() throws -> Any?] = []
        operations.append { [host] in
          return try host.tracks()
        }
        host.enqueue(operations: operations, result: result, label: call.method)
      case "screenshot":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let view = try optionalVideoView(from: args, host: host)
        let width = int64Value(args["width"]).map(Int.init)
        let height = int64Value(args["height"]).map(Int.init)
        host.enqueue(result: { [weak host, weak view] value in
          if let error = value as? FlutterError { result(error); return }
          let bytes = value as? Data ?? (view ?? host?.snapshotView())?.pngSnapshotData()
          result(bytes.map { FlutterStandardTypedData(bytes: $0) })
        }) {
          host.screenshot(width: width, height: height)
        }
      case "attachView":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let viewId = try requiredInt64(args["viewId"], name: "viewId")
        guard let view = views[viewId]?.view else {
          throw ErikaPluginError.viewNotFound(viewId)
        }
        try host.attach(view: view)
        host.completeSurface(result: result, value: nil)
      case "detachView":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let viewId = try requiredInt64(args["viewId"], name: "viewId")
        host.detach(viewId: viewId)
        host.completeSurface(result: result, value: nil)
      case "attachOverlay":
        let args = try dictionaryArgs(call.arguments)
        let host = try playerHost(from: args)
        let overlay = try ensureWindowOverlayInstalled()
        try host.attach(view: overlay)
        host.completeSurface(result: result, value: erikaWindowHostedVideoSurfaceId)
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
          generation != activeGeneration
        {
          result(nil)
          return
        }
        host.detach(viewId: erikaWindowHostedVideoSurfaceId)
        overlay?.updateOverlayFrame(
          nil,
          visible: false,
          debugLabel: nil,
          generation: generation
        )
        host.completeSurface(result: result, value: nil)
      case "setOverlayFrame":
        let args = try dictionaryArgs(call.arguments)
        let overlay = try ensureWindowOverlayInstalled()
        let visible = boolValue(args["visible"]) ?? true
        let frame = convertedOverlayRect(from: args, targetView: overlay)
        overlay.updateOverlayFrame(
          frame,
          visible: visible,
          debugLabel: args["debugLabel"] as? String,
          generation: int64Value(args["generation"])
        )
        result(nil)
      default:
        result(FlutterMethodNotImplemented)
      }
    } catch {
      result(Self.flutterError(error))
    }
  }

  public func onListen(withArguments arguments: Any?, eventSink events: @escaping FlutterEventSink) -> FlutterError? {
    subscription += 1
    sharedEventSink = events
    startPollTimerIfNeeded()
    return nil
  }

  public func onCancel(withArguments arguments: Any?) -> FlutterError? {
    sharedEventSink = nil
    pollTimer?.invalidate()
    pollTimer = nil
    return nil
  }

  fileprivate func registerView(_ view: ErikaMetalSurfaceView, viewId: Int64) {
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

  private func ensureWindowOverlayInstalled() throws -> ErikaWindowOverlayView {
    if let existing = resolveWindowOverlay(),
       existing.superview != nil {
      return existing
    }

    guard let flutterHostView = currentFlutterHostView() else {
      throw ErikaPluginError.overlayNotAvailable
    }
    guard let hostWindow = flutterHostView.window else {
      throw ErikaPluginError.overlayNotAvailable
    }
    let hostSuperview = flutterHostView.superview ?? hostWindow

    prepareFlutterHostViewForWindowOverlay(flutterHostView)

    let overlay = hostWindow.erikaWindowOverlayView ??
      ErikaWindowOverlayView(plugin: self)
    overlay.plugin = self

    if overlay.superview !== hostSuperview {
      overlay.removeFromSuperview()
      overlay.frame = .zero
    }
    if flutterHostView.superview === hostSuperview,
       shouldPlaceWindowOverlayAboveFlutter() {
      hostSuperview.insertSubview(overlay, aboveSubview: flutterHostView)
    } else if flutterHostView.superview === hostSuperview {
      hostSuperview.insertSubview(overlay, belowSubview: flutterHostView)
    } else if shouldPlaceWindowOverlayAboveFlutter() {
      hostSuperview.addSubview(overlay)
    } else {
      hostSuperview.insertSubview(overlay, at: 0)
    }

    hostWindow.erikaWindowOverlayView = overlay
    registerView(overlay, viewId: overlay.platformViewId)
    return overlay
  }

  private func resolveWindowOverlay() -> ErikaWindowOverlayView? {
    for window in activeWindows() {
      if let overlay = window.erikaWindowOverlayView {
        return overlay
      }
    }
    return nil
  }

  private func currentFlutterHostView() -> UIView? {
    for window in activeWindows() {
      if let controller = findFlutterViewController(from: window.rootViewController) {
        return controller.view
      }
    }
    return activeWindows().first?.rootViewController?.view
  }

  private func activeWindows() -> [UIWindow] {
    let scenes = UIApplication.shared.connectedScenes
      .compactMap { $0 as? UIWindowScene }
      .filter {
        $0.activationState == .foregroundActive ||
          $0.activationState == .foregroundInactive
      }
    let windows = scenes.flatMap(\.windows).filter { !$0.isHidden }
    return windows.sorted { lhs, rhs in
      if lhs.isKeyWindow != rhs.isKeyWindow {
        return lhs.isKeyWindow
      }
      return lhs.windowLevel.rawValue > rhs.windowLevel.rawValue
    }
  }

  private func findFlutterViewController(from controller: UIViewController?) -> FlutterViewController? {
    guard let controller else {
      return nil
    }
    if let flutter = controller as? FlutterViewController {
      return flutter
    }
    if let presented = findFlutterViewController(from: controller.presentedViewController) {
      return presented
    }
    if let navigation = controller as? UINavigationController,
       let visible = findFlutterViewController(from: navigation.visibleViewController) {
      return visible
    }
    if let tab = controller as? UITabBarController,
       let selected = findFlutterViewController(from: tab.selectedViewController) {
      return selected
    }
    for child in controller.children {
      if let flutter = findFlutterViewController(from: child) {
        return flutter
      }
    }
    return nil
  }

  private func prepareFlutterHostViewForWindowOverlay(_ view: UIView) {
    if shouldPlaceWindowOverlayAboveFlutter() {
      return
    }
    view.isOpaque = false
    view.backgroundColor = .clear
    view.layer.isOpaque = false
    view.layer.backgroundColor = UIColor.clear.cgColor
    view.window?.backgroundColor = .black
  }

  private func shouldPlaceWindowOverlayAboveFlutter() -> Bool {
    let environment = ProcessInfo.processInfo.environment
    if environment["ERIKA_WINDOW_OVERLAY_BELOW"] == "1" {
      return false
    }
    return environment["ERIKA_WINDOW_OVERLAY_ABOVE"] == "1"
  }

  private func convertedOverlayRect(
    from args: [String: Any],
    targetView: UIView
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
    guard let flutterHostView = currentFlutterHostView(),
          let targetSuperview = targetView.superview else {
      return CGRect(x: x, y: y, width: width, height: height)
    }
    let rect = CGRect(x: x, y: y, width: width, height: height)
    return flutterHostView.convert(rect, to: targetSuperview)
  }

  private func createPlayer(arguments: Any?) throws -> Int64 {
    let args = arguments as? [String: Any]
    let hdrDebug = boolValue(args?["hdrDebug"]) ??
      boolEnvironmentFlag("ERIKA_HDR_DEBUG", environment: ProcessInfo.processInfo.environment)
    let config = presenterConfigForNewPlayer(arguments: arguments, hdrDebug: hdrDebug)
    let id = nextPlayerId
    nextPlayerId += 1
    let host = try ErikaPlayerHost(
      id: id,
      config: config,
      hdrDebug: hdrDebug,
      allowBackgroundPlayback: boolValue(args?["allowBackgroundPlayback"]) ?? false
    )
    host.onNowPlayingChanged = { [weak self] changedHost in
      guard self?.activePlayerId == changedHost.id else { return }
      self?.updateNowPlayingInfo(for: changedHost)
    }
    players[id] = host
    systemMediaNavigation[id] = (previousEnabled: false, nextEnabled: false)
    startPollTimerIfNeeded()
    return id
  }

  private func configureSystemPlayback() {
    let center = NotificationCenter.default
    notificationObservers.append(center.addObserver(
      forName: UIApplication.willResignActiveNotification,
      object: nil,
      queue: .main
    ) { [weak self] _ in
      self?.players.values.forEach { $0.prepareForInactiveApp() }
    })
    notificationObservers.append(center.addObserver(
      forName: UIApplication.didEnterBackgroundNotification,
      object: nil,
      queue: .main
    ) { [weak self] _ in
      self?.players.values.forEach { $0.didEnterBackground() }
    })
    notificationObservers.append(center.addObserver(
      forName: UIApplication.didBecomeActiveNotification,
      object: nil,
      queue: .main
    ) { [weak self] _ in
      self?.players.values.forEach { $0.setAppInBackground(false) }
    })
    notificationObservers.append(center.addObserver(
      forName: AVAudioSession.interruptionNotification,
      object: AVAudioSession.sharedInstance(),
      queue: .main
    ) { [weak self] notification in
      self?.handleAudioInterruption(notification)
    })

    let commands = MPRemoteCommandCenter.shared()
    addRemoteTarget(commands.playCommand) { [weak self] _ in self?.performRemotePlay() ?? .commandFailed }
    addRemoteTarget(commands.pauseCommand) { [weak self] _ in self?.performRemotePause() ?? .commandFailed }
    addRemoteTarget(commands.togglePlayPauseCommand) { [weak self] _ in self?.performRemoteToggle() ?? .commandFailed }
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

  private func updateNowPlayingInfo(for host: ErikaPlayerHost) {
    var info: [String: Any] = [
      MPMediaItemPropertyTitle: host.nowPlayingTitle,
      MPNowPlayingInfoPropertyElapsedPlaybackTime: host.positionSeconds,
      MPNowPlayingInfoPropertyPlaybackRate: host.isPlaying ? host.playbackRate : 0,
      MPNowPlayingInfoPropertyDefaultPlaybackRate: host.playbackRate,
      MPNowPlayingInfoPropertyMediaType: MPNowPlayingInfoMediaType.video.rawValue,
    ]
    if let artist = host.nowPlayingArtist { info[MPMediaItemPropertyArtist] = artist }
    if let album = host.nowPlayingAlbum { info[MPMediaItemPropertyAlbumTitle] = album }
    if let artwork = host.nowPlayingArtwork { info[MPMediaItemPropertyArtwork] = artwork }
    if let duration = host.durationSeconds { info[MPMediaItemPropertyPlaybackDuration] = duration }
    let center = MPNowPlayingInfoCenter.default()
    center.nowPlayingInfo = info
    center.playbackState = host.isPlaying ? .playing : .paused
  }

  private func clearNowPlayingInfo() {
    let center = MPNowPlayingInfoCenter.default()
    center.nowPlayingInfo = nil
    center.playbackState = .stopped
  }

  private func performRemotePlay() -> MPRemoteCommandHandlerStatus {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in _ = self?.performRemotePlay() }
      return .success
    }
    guard let host = activePlayerId.flatMap({ players[$0] }) else { return .noSuchContent }
    do {
      try host.play()
      return .success
    } catch {
      return .commandFailed
    }
  }

  private func performRemotePause() -> MPRemoteCommandHandlerStatus {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in _ = self?.performRemotePause() }
      return .success
    }
    guard let host = activePlayerId.flatMap({ players[$0] }) else { return .noSuchContent }
    cancelPendingInterruptionResume(ifPlayer: host.id)
    do {
      try host.pause()
      return .success
    } catch {
      return .commandFailed
    }
  }

  private func performRemoteToggle() -> MPRemoteCommandHandlerStatus {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in _ = self?.performRemoteToggle() }
      return .success
    }
    guard let host = activePlayerId.flatMap({ players[$0] }) else { return .noSuchContent }
    return host.isPlaying ? performRemotePause() : performRemotePlay()
  }

  private func performRemoteSeek(_ position: TimeInterval) -> MPRemoteCommandHandlerStatus {
    if !Thread.isMainThread {
      DispatchQueue.main.async { [weak self] in _ = self?.performRemoteSeek(position) }
      return .success
    }
    guard let host = activePlayerId.flatMap({ players[$0] }) else { return .noSuchContent }
    do {
      try host.seek(positionMicros: UInt64(max(0, position) * 1_000_000))
      return .success
    } catch {
      return .commandFailed
    }
  }

  private func emitSystemMediaNavigation(_ navigation: String) -> MPRemoteCommandHandlerStatus {
    performOnMain { [weak self] in
      guard let self, self.delivery.valid, let playerId = self.activePlayerId,
            self.players[playerId] != nil,
            let capabilities = self.systemMediaNavigation[playerId] else { return .noSuchContent }
      let enabled = navigation == "previous"
        ? capabilities.previousEnabled
        : capabilities.nextEnabled
      guard enabled else { return .noSuchContent }
      self.sharedEventSink?([
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
    DispatchQueue.main.async { _ = work() }
    return .success
  }

  private func refreshRemoteCommands() {
    let commands = MPRemoteCommandCenter.shared()
    let enabled = activePlayerId.flatMap { players[$0] } != nil
    remoteCommandTargets.forEach { command, _ in
      command.isEnabled = enabled
    }
    let capabilities = activePlayerId.flatMap { systemMediaNavigation[$0] }
    commands.previousTrackCommand.isEnabled = enabled && capabilities?.previousEnabled == true
    commands.nextTrackCommand.isEnabled = enabled && capabilities?.nextEnabled == true
  }

  private func handleAudioInterruption(_ notification: Notification) {
    guard let rawType = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
          let type = AVAudioSession.InterruptionType(rawValue: rawType),
          let host = activePlayerId.flatMap({ players[$0] }) else { return }
    if type == .began {
      let wasPlaying = host.isPlaying
      cancelPendingInterruptionResume()
      interruptedPlayerId = wasPlaying ? host.id : nil
      if wasPlaying {
        do {
          try host.pause()
        } catch {
          NSLog("ErikaFlutterPlugin: audio interruption pause failed: \(error)")
        }
      }
      return
    }
    guard let rawOptions = notification.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt,
          AVAudioSession.InterruptionOptions(rawValue: rawOptions).contains(.shouldResume),
          interruptedPlayerId == host.id else {
      cancelPendingInterruptionResume()
      return
    }
    resumeInterruptedPlayback(host, attempt: 0)
  }

  /// Drops a scheduled interruption resume, whichever player it targets.
  private func cancelPendingInterruptionResume() {
    interruptionResumeWorkItem?.cancel()
    interruptionResumeWorkItem = nil
    interruptedPlayerId = nil
  }

  /// Drops a scheduled interruption resume only when it targets `playerId`, so
  /// an explicit pause/stop/close on one player leaves another player's
  /// pending resume alone.
  private func cancelPendingInterruptionResume(ifPlayer playerId: Int64) {
    guard interruptedPlayerId == playerId else { return }
    cancelPendingInterruptionResume()
  }

  private func resumeInterruptedPlayback(_ host: ErikaPlayerHost, attempt: Int) {
    guard interruptedPlayerId == host.id, activePlayerId == host.id else {
      cancelPendingInterruptionResume(ifPlayer: host.id)
      return
    }
    do {
      try host.prepareAudioSession()
      host.enqueue(result: { [weak self, weak host] value in
        guard let self, let host, self.delivery.valid,
              self.interruptedPlayerId == host.id, self.activePlayerId == host.id else { return }
        if let error = value as? FlutterError {
          self.retryInterruptedPlayback(host, attempt: attempt, error: error)
        } else { self.cancelPendingInterruptionResume() }
      }) { try host.play(); return nil }
    } catch { retryInterruptedPlayback(host, attempt: attempt, error: error) }
  }

  private func retryInterruptedPlayback(_ host: ErikaPlayerHost, attempt: Int, error: Any) {
    let maxAttempts = 3
    guard attempt < maxAttempts else {
      cancelPendingInterruptionResume()
      NSLog("ErikaFlutterPlugin: audio interruption resume failed after \(maxAttempts + 1) attempts: \(error)")
      return
    }
    let workItem = DispatchWorkItem { [weak self, weak host] in
      guard let self, let host else { return }
      self.interruptionResumeWorkItem = nil
      self.resumeInterruptedPlayback(host, attempt: attempt + 1)
    }
    interruptionResumeWorkItem = workItem
    DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(100), execute: workItem)
  }

  private func presenterConfigForNewPlayer(arguments: Any?, hdrDebug: Bool) -> ErikaPresenterConfigC {
    // An explicit disable must become SDR, not `auto(headroom: 1.0)`: under
    // the per-presenting-display Auto negotiation a headroom of 1.0 means "no
    // embedder cap, defer to the display", so an EDR display would promote
    // again and ignore ERIKA_DISABLE_EDR.
    let edrDisabled = boolEnvironmentFlag(
      "ERIKA_DISABLE_EDR",
      environment: ProcessInfo.processInfo.environment
    )
    if let args = arguments as? [String: Any], let explicitMode = int32Value(args["outputMode"]) {
      let headroom = floatValue(args["edrHeadroom"]) ?? 4.0
      let config: ErikaPresenterConfigC
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
      erikaHdrLog(
        hdrDebug,
        "create explicit outputMode=\(explicitMode) requestedHeadroom=\(String(format: "%.3f", headroom)) selected=\(erikaOutputModeLabel(config))"
      )
      return config
    }
    guard let headroom = resolvedEdrHeadroom(hdrDebug: hdrDebug) else {
      erikaHdrLog(hdrDebug, "ERIKA_DISABLE_EDR is set; using SDR output")
      return .sdr
    }
    let config = ErikaPresenterConfigC.auto(headroom: headroom)
    erikaHdrLog(
      hdrDebug,
      "create auto selected=\(erikaOutputModeLabel(config)) resolvedHeadroom=\(String(format: "%.3f", headroom))"
    )
    return config
  }

  /// EDR headroom to request for a new player, or `nil` when
  /// `ERIKA_DISABLE_EDR` disables EDR and the player must use SDR.
  private func resolvedEdrHeadroom(hdrDebug: Bool) -> Float? {
    let environment = ProcessInfo.processInfo.environment
    if boolEnvironmentFlag("ERIKA_DISABLE_EDR", environment: environment) {
      erikaHdrLog(hdrDebug, "EDR disabled by ERIKA_DISABLE_EDR")
      return nil
    }
    if let override = floatEnvironmentValue("ERIKA_EDR_HEADROOM", environment: environment), override > 1.0 {
      erikaHdrLog(hdrDebug, "EDR headroom override ERIKA_EDR_HEADROOM=\(String(format: "%.3f", override))")
      return override
    }
    let screenHeadroom = currentScreenEdrHeadroom(hdrDebug: hdrDebug)
    if screenHeadroom > 1.0 { return screenHeadroom }
    if boolEnvironmentFlag("ERIKA_ENABLE_EDR", environment: environment) {
      erikaHdrLog(hdrDebug, "EDR forced by ERIKA_ENABLE_EDR")
      return 4.0
    }
    return 1.0
  }

  private func currentScreenEdrHeadroom(hdrDebug: Bool) -> Float {
    let screen = UIScreen.main
    var samples: [String] = []
    for key in ["potentialEDRHeadroom", "currentEDRHeadroom", "maximumPotentialExtendedDynamicRangeColorComponentValue"] {
      let selector = Selector(key)
      if screen.responds(to: selector), let number = screen.value(forKey: key) as? NSNumber {
        let value = number.floatValue
        samples.append("\(key)=\(String(format: "%.3f", value))")
        if value.isFinite && value > 1.0 {
          erikaHdrLog(
            hdrDebug,
            "screen headroom selected \(key)=\(String(format: "%.3f", value)) \(erikaScreenSummary(screen)) samples=[\(samples.joined(separator: ", "))]"
          )
          return value
        }
      } else {
        samples.append("\(key)=unavailable")
      }
    }
    erikaHdrLog(
      hdrDebug,
      "screen headroom fallback=1.000 \(erikaScreenSummary(screen)) samples=[\(samples.joined(separator: ", "))]"
    )
    return 1.0
  }

  private func startPollTimerIfNeeded() {
    guard pollTimer == nil else { return }
    let timer = Timer(timeInterval: 0.05, repeats: true) { [weak self] _ in
      guard let self else { return }
      let generation = self.subscription
      let sink: ([String: Any]) -> Void = { [weak self] event in
        guard let self, self.delivery.valid, self.subscription == generation else { return }
        self.sharedEventSink?(event)
      }
      for host in self.players.values {
        host.pollEvents(sendEvent: sink)
      }
    }
    pollTimer = timer
    RunLoop.main.add(timer, forMode: .common)
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
  ) throws -> ErikaMetalSurfaceView? {
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
    if value == nil || value is NSNull { return nil }
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
    if value == nil || value is NSNull { return 0 }
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

  private func danmakuConfigPatch(from args: [String: Any]) -> (ErikaDanmakuConfigC) -> ErikaDanmakuConfigC {
    let enabled = boolValue(args["enabled"])
    let fontSize = doubleValue(args["fontSize"])
    let opacity = doubleValue(args["opacity"])
    let displayArea = doubleValue(args["displayArea"])
    let scrollDurationSeconds = doubleValue(args["scrollDurationSeconds"])
    let scrollSpeedFactor = doubleValue(args["scrollSpeedFactor"])
    let trackGapRatio = doubleValue(args["trackGapRatio"])
    let outlineWidth = doubleValue(args["outlineWidth"])
    let shadowOffsetX = doubleValue(args["shadowOffsetX"])
    let shadowOffsetY = doubleValue(args["shadowOffsetY"])
    let mergeDuplicates = boolValue(args["mergeDuplicates"])
    let allowStacking = boolValue(args["allowStacking"])
    let allowScrollOverwrite = boolValue(args["allowScrollOverwrite"])
    let maxQuantity = int64Value(args["maxQuantity"])
    let maxLinesPerMode = int64Value(args["maxLinesPerMode"])
    let blockTop = boolValue(args["blockTop"])
    let blockBottom = boolValue(args["blockBottom"])
    let blockScroll = boolValue(args["blockScroll"])
    let shadowStyle = int64Value(args["shadowStyle"])
    return { base in
      var config = base
      if let value = enabled { config.enabled = value ? 1 : 0 }
      if let value = fontSize { config.fontSize = Float(value) }
      if let value = opacity { config.opacity = Float(value) }
      if let value = displayArea { config.displayArea = Float(value) }
      if let value = scrollDurationSeconds { config.scrollDurationSeconds = Float(value) }
      if let value = scrollSpeedFactor { config.scrollSpeedFactor = Float(value) }
      if let value = trackGapRatio { config.trackGapRatio = Float(value) }
      if let value = outlineWidth { config.outlineWidth = Float(value) }
      if let value = shadowOffsetX { config.shadowOffsetX = Float(value) }
      if let value = shadowOffsetY { config.shadowOffsetY = Float(value) }
      if let value = mergeDuplicates { config.mergeDuplicates = value ? 1 : 0 }
      if let value = allowStacking { config.allowStacking = value ? 1 : 0 }
      if let value = allowScrollOverwrite { config.allowScrollOverwrite = value ? 1 : 0 }
      if let value = maxQuantity, value > 0 { config.maxQuantity = UInt32(clamping: value) }
      if let value = maxLinesPerMode, value > 0 { config.maxLinesPerMode = UInt32(clamping: value) }
      if let value = blockTop { config.blockTop = value ? 1 : 0 }
      if let value = blockBottom { config.blockBottom = value ? 1 : 0 }
      if let value = blockScroll { config.blockScroll = value ? 1 : 0 }
      if let value = shadowStyle { config.shadowStyle = Int32(clamping: value) }
      return config
    }
  }

  private func dictionaryArgs(_ arguments: Any?) throws -> [String: Any] {
    guard let args = arguments as? [String: Any] else {
      throw ErikaPluginError.invalidArguments("Arguments must be a dictionary.")
    }
    return args
  }

  private func int32Value(_ value: Any?) -> Int32? {
    if let value = value as? Int32 { return value }
    if let value = value as? NSNumber { return value.int32Value }
    if let value = value as? String { return Int32(value) }
    return nil
  }

  private func int64Value(_ value: Any?) -> Int64? {
    if let value = value as? Int64 { return value }
    if let value = value as? NSNumber { return value.int64Value }
    if let value = value as? String { return Int64(value) }
    return nil
  }

  private func doubleValue(_ value: Any?) -> Double? {
    if let value = value as? Double { return value }
    if let value = value as? NSNumber { return value.doubleValue }
    if let value = value as? String { return Double(value) }
    return nil
  }

  private func floatValue(_ value: Any?) -> Float? {
    if let value = value as? Float, value.isFinite { return value }
    if let value = value as? Double, value.isFinite { return Float(value) }
    if let value = value as? NSNumber {
      let result = value.floatValue
      return result.isFinite ? result : nil
    }
    if let value = value as? String, let result = Float(value), result.isFinite { return result }
    return nil
  }

  private func boolValue(_ value: Any?) -> Bool? {
    if let value = value as? Bool { return value }
    if let value = value as? NSNumber { return value.boolValue }
    if let value = value as? String {
      switch value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
      case "1", "true", "yes", "on": return true
      case "0", "false", "no", "off": return false
      default: return nil
      }
    }
    return nil
  }

  private func requiredInt64(_ value: Any?, name: String) throws -> Int64 {
    if let value = int64Value(value) { return value }
    throw ErikaPluginError.invalidArguments("\(name) is required.")
  }

  private func requiredUInt64(_ value: Any?, name: String) throws -> UInt64 {
    if let value = value as? UInt64 { return value }
    if let value = value as? Int64, value >= 0 { return UInt64(value) }
    if let value = value as? NSNumber { return value.uint64Value }
    if let value = value as? String, let parsed = UInt64(value) { return parsed }
    throw ErikaPluginError.invalidArguments("\(name) is required.")
  }

  private func boolEnvironmentFlag(_ name: String, environment: [String: String]) -> Bool {
    switch environment[name]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
    case "1", "true", "yes", "on": return true
    default: return false
    }
  }

  private func floatEnvironmentValue(_ name: String, environment: [String: String]) -> Float? {
    guard let raw = environment[name]?.trimmingCharacters(in: .whitespacesAndNewlines),
          !raw.isEmpty,
          let value = Float(raw),
          value.isFinite else {
      return nil
    }
    return value
  }

  /// Runs `work` on `queue`, then answers on the main thread, which is where
  /// Flutter wants results. `keeping` stays referenced until the answer, so
  /// the last reference to a player never drops on a background queue (its
  /// teardown touches the display link).
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
          result(Self.flutterError(error))
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

  private static func flutterError(_ error: Error) -> FlutterError {
    if let pluginError = error as? ErikaPluginError,
       case .diagnosticsUnsupported = pluginError {
      return FlutterError(code: "ERIKA_UNSUPPORTED", message: String(describing: error), details: nil)
    }
    return FlutterError(code: "ERIKA_ERROR", message: String(describing: error), details: nil)
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
    ["video": Int(video), "audio": Int(audio), "subtitle": Int(subtitle)]
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
  return value.withCString { pointer in body(pointer) }
}
