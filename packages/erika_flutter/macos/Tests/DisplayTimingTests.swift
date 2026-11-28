import CoreVideo
import FlutterMacOS
import QuartzCore
import XCTest
@testable import erika_flutter

final class DisplayTimingTests: XCTestCase {
  func testHostTimeConversionPreservesFutureAndLateTargets() {
    var timestamp = CVTimeStamp()
    timestamp.flags = CVTimeStampFlags.hostTimeValid.rawValue
    // Large absolute counts exercise integer subtraction before conversion.
    let host: UInt64 = 1 << 60
    timestamp.hostTime = host + 16_000
    XCTAssertEqual(erikaDisplayPresentationTime(timestamp, hostTime: host,
      hostFrequency: 1_000_000, now: 10)!, 10.016, accuracy: 1e-12)
    timestamp.hostTime = host - 9_000
    XCTAssertEqual(erikaDisplayPresentationTime(timestamp, hostTime: host,
      hostFrequency: 1_000_000, now: 10)!, 9.991, accuracy: 1e-12)
  }

  func testUnavailableTimestampFallsBackToUntimedRendering() {
    var timestamp = CVTimeStamp()
    XCTAssertNil(erikaDisplayPresentationTime(timestamp, hostTime: 0,
      hostFrequency: 1_000_000, now: 10))
    timestamp.flags = CVTimeStampFlags.hostTimeValid.rawValue
    for frequency in [0.0, -1.0, Double.nan, Double.infinity] {
      XCTAssertNil(erikaDisplayPresentationTime(timestamp, hostTime: 0,
        hostFrequency: frequency, now: 10))
    }
  }

  func testOwnerMailboxKeepsNewestAbsoluteDisplayTarget() {
    let owner = ErikaPresenterOwner(id: -216)
    let blocked = expectation(description: "owner blocked")
    let rendered = expectation(description: "latest target rendered")
    let closed = expectation(description: "owner closed")
    let gate = DispatchSemaphore(value: 0)
    owner.post {
      blocked.fulfill()
      gate.wait()
    }
    wait(for: [blocked], timeout: 2)
    for target in [10.016, 10.032, 10.048] {
      owner.requestFrame {
        XCTAssertEqual(target, 10.048)
        rendered.fulfill()
      }
    }
    gate.signal()
    wait(for: [rendered], timeout: 2)
    owner.finish { closed.fulfill() }
    wait(for: [closed], timeout: 2)
  }
}

private final class TimingTestTextures: NSObject, FlutterTextureRegistry {
  func register(_ texture: FlutterTexture) -> Int64 { 1 }
  func textureFrameAvailable(_ textureId: Int64) {}
  func unregisterTexture(_ textureId: Int64) {}
}

extension DisplayTimingTests {
  func testPluginForwardsDisplayTargetToTimedNativeEntry() throws {
    let path = try XCTUnwrap(ProcessInfo.processInfo.environment["ERIKA_CAPI_DYLIB"])
    let library = try XCTUnwrap(dlopen(path, RTLD_NOW | RTLD_GLOBAL))
    defer { dlclose(library) }
    let symbol = try XCTUnwrap(dlsym(library, "erika_test_count"))
    let count = unsafeBitCast(symbol, to: (@convention(c) (Int32) -> Int32).self)
    let plugin = ErikaFlutterPlugin(flutterHostView: nil,
      flutterHostViewController: nil, textureRegistry: TimingTestTextures())
    defer { plugin.shutdownForTesting() }
    var playerId: Int64?
    let created = expectation(description: "player created")
    plugin.handle(FlutterMethodCall(methodName: "create", arguments: ["outputMode": 0])) {
      playerId = $0 as? Int64
      created.fulfill()
    }
    wait(for: [created], timeout: 2)
    let id = try XCTUnwrap(playerId)
    let before = count(5)
    let target = CACurrentMediaTime() + 0.2
    plugin.requestFrameForTesting(playerId: id, presentationTime: target)
    let rendered = expectation(description: "timed native call")
    let timer = Timer.scheduledTimer(withTimeInterval: 0.001, repeats: true) { timer in
      if count(5) > before {
        timer.invalidate()
        rendered.fulfill()
      }
    }
    wait(for: [rendered], timeout: 2)
    timer.invalidate()
    XCTAssertEqual(count(5), before + 1)
    let observedDelay = Double(count(6)) / 1_000_000
    XCTAssertLessThanOrEqual(observedDelay, 0.2)
    XCTAssertGreaterThanOrEqual(observedDelay, target - CACurrentMediaTime() - 0.001)

    // An untimed tick uses the compatibility entry; a grossly stale display
    // target is dropped. A subsequent owner command lets both requests drain.
    for (index, presentationTime) in ([nil, CACurrentMediaTime() - 1] as [Double?]).enumerated() {
      plugin.requestFrameForTesting(playerId: id, presentationTime: presentationTime)
      let drained = expectation(description: "owner drained")
      plugin.handle(FlutterMethodCall(methodName: "setVolume",
        arguments: ["playerId": id, "volume": Double(index)])) { _ in drained.fulfill() }
      wait(for: [drained], timeout: 2)
    }
    XCTAssertEqual(count(5), before + 1)
  }
}
