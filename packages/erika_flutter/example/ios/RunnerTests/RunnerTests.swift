import Foundation
import XCTest

@testable import erika_flutter

#if os(macOS)
  import FlutterMacOS
#else
  import Flutter
#endif

#if os(macOS)
  private final class TestTextures: NSObject, FlutterTextureRegistry {
    weak var registered: NSObject?
    private(set) var unregistered = 0
    func register(_ texture: FlutterTexture) -> Int64 {
      XCTAssertTrue(Thread.isMainThread)
      registered = texture as? NSObject
      return 1
    }
    func textureFrameAvailable(_ textureId: Int64) { XCTAssertTrue(Thread.isMainThread) }
    func unregisterTexture(_ textureId: Int64) {
      XCTAssertTrue(Thread.isMainThread)
      XCTAssertNotNil(registered, "texture must live through unregister")
      unregistered += 1
    }
  }
#endif

/// Run with NativeStub.c linked/loaded through ErikaNativeLibrary. These tests
/// never create a Flutter engine, window, or real GPU/native renderer.
final class RunnerTests: XCTestCase {
  private func plugin() -> ErikaFlutterPlugin {
    #if os(macOS)
      return ErikaFlutterPlugin(
        flutterHostView: nil, flutterHostViewController: nil, textureRegistry: TestTextures())
    #else
      return ErikaFlutterPlugin()
    #endif
  }

  private func call(_ plugin: ErikaFlutterPlugin, _ method: String, _ args: [String: Any] = [:])
    -> Any?
  {
    let done = expectation(description: method)
    var value: Any?
    let arguments = method == "create" ? ["outputMode": 0] : args
    plugin.handle(FlutterMethodCall(methodName: method, arguments: arguments)) {
      XCTAssertTrue(Thread.isMainThread)
      value = $0
      done.fulfill()
    }
    wait(for: [done], timeout: 10)
    XCTAssertFalse(value is FlutterError, "\(method): \(String(describing: value))")
    return value
  }

  private func eventually(_ check: @escaping () -> Bool) {
    let done = expectation(description: "native completion")
    let timer = Timer.scheduledTimer(withTimeInterval: 0.01, repeats: true) { timer in
      if check() {
        timer.invalidate()
        done.fulfill()
      }
    }
    wait(for: [done], timeout: 10)
    timer.invalidate()
  }

  private func stub() throws -> (
    count: @convention(c) (Int32) -> Int32, hold: @convention(c) (Int32) -> Void
  ) {
    let path = ProcessInfo.processInfo.environment["ERIKA_CAPI_DYLIB"]
    let library = path.flatMap { dlopen($0, RTLD_NOW | RTLD_GLOBAL) } ?? dlopen(nil, RTLD_NOW)
    guard let library,
      let count = dlsym(library, "erika_test_count"),
      let hold = dlsym(library, "erika_test_hold")
    else {
      throw XCTSkip(
        "NativeStub.c is required for plugin ownership tests; see macos/Tests/README.md")
    }
    return (
      unsafeBitCast(count, to: (@convention(c) (Int32) -> Int32).self),
      unsafeBitCast(hold, to: (@convention(c) (Int32) -> Void).self)
    )
  }

  func testOwnerFIFOCoalescingAndFairFrames() {
    let owner = ErikaPresenterOwner(id: -100)
    let entered = expectation(description: "blocked command started")
    let drained = expectation(description: "commands drained")
    let destroyed = expectation(description: "owner finished")
    let release = DispatchSemaphore(value: 0)
    var order: [Int] = []
    var renderedTargets: [Int] = []
    var commandsAtFrame: [Int] = []
    owner.post {
      owner.assertCurrent()
      entered.fulfill()
      release.wait()
    }
    wait(for: [entered], timeout: 10)
    for index in 0..<500 {
      owner.post {
        owner.assertCurrent()
        order.append(index)
      }
    }
    for target in 0..<20_000 {
      owner.requestFrame {
        owner.assertCurrent()
        renderedTargets.append(target)
        commandsAtFrame.append(order.count)
      }
    }
    owner.post {
      XCTAssertEqual(order, Array(0..<500))
      XCTAssertEqual(renderedTargets, [19_999])
      XCTAssertEqual(commandsAtFrame, [0])
      drained.fulfill()
    }
    DispatchQueue.global().async { release.signal() }
    wait(for: [drained], timeout: 10)
    owner.finish {
      owner.assertCurrent()
      destroyed.fulfill()
    }
    XCTAssertFalse(owner.post { XCTFail("accepted after finish") })
    wait(for: [destroyed], timeout: 10)
  }

  func testCompletionOrderingInvalidationAndIndependentPlayers() {
    let delivery = ErikaPlatformDelivery()
    var values: [Int] = []
    let first = delivery.reserve(key: 1) { values.append($0 as! Int) }
    let second = delivery.reserve(key: 1) { values.append($0 as! Int) }
    let independent = delivery.reserve(key: 2) { values.append($0 as! Int) }
    second(2)
    XCTAssertTrue(values.isEmpty)
    independent(3)
    XCTAssertEqual(values, [3])
    first(1)
    XCTAssertEqual(values, [3, 1, 2])
    let late = delivery.reserve { _ in XCTFail("called retired messenger") }
    delivery.invalidate()
    late(nil)
  }

  func testPluginCommandsTicksCaptureAndConfigPatches() throws {
    let control = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    let startTicks = control.count(2)
    let startCommands = control.count(3)
    let startEntered = control.count(4)
    control.hold(1)
    defer { control.hold(0) }
    let all = expectation(description: "ordered native results")
    all.expectedFulfillmentCount = 201
    var delivered: [Int] = []
    let record: (Int) -> FlutterResult = { index in
      { value in
        XCTAssertTrue(Thread.isMainThread)
        XCTAssertFalse(value is FlutterError)
        delivered.append(index)
        all.fulfill()
      }
    }
    plugin.handle(
      FlutterMethodCall(methodName: "open", arguments: ["playerId": id, "uri": "stub://slow"]),
      result: record(0))
    eventually { control.count(4) > startEntered }
    for index in 1...200 {
      plugin.handle(
        FlutterMethodCall(
          methodName: "seek", arguments: ["playerId": id, "positionMicros": index]),
        result: record(index))
    }
    for _ in 0..<20_000 { plugin.requestFrameForTesting(playerId: id) }
    XCTAssertTrue(delivered.isEmpty, "platform must return while native open is blocked")
    DispatchQueue.global().async { control.hold(0) }
    wait(for: [all], timeout: 10)
    XCTAssertEqual(delivered, Array(0...200))
    XCTAssertEqual(control.count(2) - startTicks, 1, "stale ticks must coalesce")
    XCTAssertEqual(control.count(3) - startCommands, 201)
    let data =
      call(plugin, "screenshot", ["playerId": id, "width": 2, "height": 2])
      as! FlutterStandardTypedData
    XCTAssertEqual(data.data, Data(repeating: 127, count: 16))
    _ = call(plugin, "setDanmakuConfig", ["playerId": id, "fontSize": 42])
    _ = call(plugin, "setDanmakuConfig", ["playerId": id, "opacity": 0.25])
    let json = call(plugin, "invokeJson", ["playerId": id, "method": "config"]) as! String
    let config = try JSONSerialization.jsonObject(with: Data(json.utf8)) as! [String: Double]
    XCTAssertEqual(config["fontSize"], 42)
    XCTAssertEqual(config["opacity"], 0.25)
    _ = call(plugin, "dispose", ["playerId": id])
  }

  func testDisposeDrainsAcceptedCommandsWithoutWaitingOnMain() throws {
    let control = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    let startDestroyed = control.count(1)
    control.hold(1)
    defer { control.hold(0) }
    let all = expectation(description: "dispose after accepted results")
    all.expectedFulfillmentCount = 102
    var delivered: [Int] = []
    for index in 0...100 {
      let method = index == 0 ? "open" : "seek"
      let args: [String: Any] =
        index == 0
        ? ["playerId": id, "uri": "stub://dispose"] : ["playerId": id, "positionMicros": index]
      plugin.handle(FlutterMethodCall(methodName: method, arguments: args)) { value in
        XCTAssertTrue(Thread.isMainThread)
        XCTAssertFalse(value is FlutterError)
        delivered.append(index)
        all.fulfill()
      }
    }
    plugin.requestFrameForTesting(playerId: id)
    plugin.handle(FlutterMethodCall(methodName: "dispose", arguments: ["playerId": id])) { value in
      XCTAssertTrue(Thread.isMainThread)
      XCTAssertNil(value)
      XCTAssertEqual(control.count(1), startDestroyed + 1)
      delivered.append(101)
      all.fulfill()
    }
    XCTAssertTrue(delivered.isEmpty)
    DispatchQueue.global().async { control.hold(0) }
    wait(for: [all], timeout: 10)
    XCTAssertEqual(delivered, Array(0...101))
  }

  func testDisposeDuringNativeRender() throws {
    let control = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    let startTicks = control.count(2)
    let startDestroyed = control.count(1)
    control.hold(2)
    defer { control.hold(0) }
    plugin.requestFrameForTesting(playerId: id)
    eventually { control.count(2) > startTicks }
    let disposed = expectation(description: "dispose after in-flight render")
    plugin.handle(FlutterMethodCall(methodName: "dispose", arguments: ["playerId": id])) { value in
      XCTAssertTrue(Thread.isMainThread)
      XCTAssertNil(value)
      disposed.fulfill()
    }
    XCTAssertEqual(control.count(1), startDestroyed)
    DispatchQueue.global().async { control.hold(0) }
    wait(for: [disposed], timeout: 10)
    XCTAssertEqual(control.count(1), startDestroyed + 1)
  }

  func testOwnedModelBytesSurviveQueuedCommand() throws {
    let control = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    control.hold(1)
    defer { control.hold(0) }
    let opened = expectation(description: "queued open")
    let uploaded = expectation(description: "queued owned model bytes")
    plugin.handle(
      FlutterMethodCall(methodName: "open", arguments: ["playerId": id, "uri": "stub://bytes"])
    ) { _ in opened.fulfill() }
    var bytes = Data([1, 2, 3])
    plugin.handle(
      FlutterMethodCall(
        methodName: "loadUpscalerModel",
        arguments: ["playerId": id, "data": FlutterStandardTypedData(bytes: bytes)])
    ) { value in
      XCTAssertTrue(Thread.isMainThread)
      XCTAssertFalse(value is FlutterError)
      uploaded.fulfill()
    }
    bytes[0] = 99
    DispatchQueue.global().async { control.hold(0) }
    wait(for: [opened, uploaded], timeout: 10)
    _ = call(plugin, "dispose", ["playerId": id])
  }

  func testEveryPresenterCommandUsesTheNativeOwner() throws {
    _ = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    let commands: [(String, [String: Any])] = [
      (
        "open",
        [
          "uri": "stub://headers", "httpHeaders": ["Test": "owned"], "httpReadAheadBytes": 16,
          "httpBackBufferBytes": 8,
        ]
      ),
      ("play", [:]), ("pause", [:]), ("stop", [:]), ("close", [:]),
      ("seek", ["positionMicros": 500]), ("setPlaybackRate", ["rate": 1.25]),
      ("setVolume", ["volume": 0]), ("setUpscaler", ["mode": 0]),
      ("clearUpscalerModel", [:]), ("setSubtitleScale", ["scale": 1]),
      ("setSubtitleStyle", ["fontFamily": "stub", "fontSize": 40]),
      ("registerSubtitleMemoryFont", ["data": FlutterStandardTypedData(bytes: Data([1, 2, 3]))]),
      ("selectSubtitleMemoryFonts", ["fontIds": [0]]),
      ("clearSubtitleMemoryFonts", [:]), ("getSubtitleMemoryFontStatus", [:]),
      ("getUpscalerStatus", [:]), ("getOutputStatus", [:]),
      ("getResourceStatus", [:]), ("getDiagnosticsSnapshot", [:]),
      ("getPresenterStats", [:]), ("setDebugHudEnabled", ["enabled": true]),
      ("addExternalSubtitle", ["uri": "stub://subtitle"]),
      ("removeSubtitleTrack", ["trackId": 0]),
      ("loadDanmakuFile", ["uri": "stub://danmaku"]),
      ("loadDanmakuJson", ["json": "[]"]),
      ("addDanmakuTrackFile", ["uri": "stub://track"]),
      ("addDanmakuTrackJson", ["json": "[]"]),
      ("removeDanmakuTrack", ["trackId": 0]),
      ("setDanmakuTrackEnabled", ["trackId": 0, "enabled": true]),
      ("setDanmakuTrackOffset", ["trackId": 0, "offsetMicros": 5]),
      ("setDanmakuGlobalOffset", ["offsetMicros": 5]),
      ("danmakuTracks", [:]), ("clearDanmaku", [:]),
      ("setDanmakuEnabled", ["enabled": true]),
      ("setDanmakuConfig", ["fontSize": 42, "customFontFamily": "stub", "blockWordsJson": "[]"]),
      ("selectAudioTrack", ["trackId": 0]), ("selectSubtitleTrack", ["trackId": 0]),
      ("tracks", [:]), ("invokeJson", ["method": "config"]),
      ("setMediaMetadata", ["metadata": ["title": "stub"]]),
      ("setSystemMediaNavigation", ["previousEnabled": true, "nextEnabled": true]),
    ]
    for (method, values) in commands {
      _ = call(plugin, method, values.merging(["playerId": id]) { _, new in new })
    }
    _ = call(plugin, "dispose", ["playerId": id])
  }

  func testSurfaceTransferWaitsForNativeDetachAndSurvivesViewRelease() throws {
    let control = try stub()
    let plugin = plugin()
    let first = call(plugin, "create") as! Int64
    let second = call(plugin, "create") as! Int64
    var surface: AnyObject? = plugin.makeSurfaceForTesting(viewId: 99)
    XCTAssertNotNil(surface)
    _ = call(plugin, "attachView", ["playerId": first, "viewId": 99])
    let entered = control.count(4)
    control.hold(1)
    defer { control.hold(0) }
    let opened = expectation(description: "old owner's accepted open")
    plugin.handle(
      FlutterMethodCall(
        methodName: "open", arguments: ["playerId": first, "uri": "stub://transfer"])
    ) { value in
      XCTAssertFalse(value is FlutterError)
      opened.fulfill()
    }
    eventually { control.count(4) > entered }
    let attached = expectation(description: "transfer after old native detach")
    plugin.handle(
      FlutterMethodCall(methodName: "attachView", arguments: ["playerId": second, "viewId": 99])
    ) { value in
      XCTAssertTrue(Thread.isMainThread)
      XCTAssertFalse(value is FlutterError)
      attached.fulfill()
    }
    // Main remains free while the new owner waits for the previous owner.
    surface = nil
    // AppKit/UIKit may retain an autoreleased view until their pool drains.
    // Its caller can release it while the native transfer is still pending.
    DispatchQueue.global().async { control.hold(0) }
    wait(for: [opened, attached], timeout: 10)
    _ = call(plugin, "dispose", ["playerId": first])
    _ = call(plugin, "dispose", ["playerId": second])
  }

  func testFailedDetachDestroysBeforeSurfaceTransfer() throws {
    let control = try stub()
    let plugin = plugin()
    let first = call(plugin, "create") as! Int64
    let second = call(plugin, "create") as! Int64
    let surface = plugin.makeSurfaceForTesting(viewId: 88)
    _ = call(plugin, "attachView", ["playerId": first, "viewId": 88])
    let destroyed = control.count(1)
    control.hold(3)
    defer { control.hold(0) }
    _ = call(plugin, "attachView", ["playerId": second, "viewId": 88])
    XCTAssertEqual(control.count(1), destroyed + 1)
    _ = call(plugin, "dispose", ["playerId": first])
    _ = call(plugin, "dispose", ["playerId": second])
    withExtendedLifetime(surface) {}
  }

  #if os(macOS)
    func testTextureReleaseRetainsUntilNativeDetachAndMainUnregister() throws {
      let control = try stub()
      let registry = TestTextures()
      let plugin = ErikaFlutterPlugin(
        flutterHostView: nil, flutterHostViewController: nil, textureRegistry: registry)
      let id = call(plugin, "create") as! Int64
      let textureId = call(plugin, "createTexture", ["width": 2, "height": 2]) as! Int64
      _ = call(plugin, "attachView", ["playerId": id, "viewId": textureId])
      control.hold(1)
      defer { control.hold(0) }
      let opened = expectation(description: "texture owner command drained")
      plugin.handle(
        FlutterMethodCall(methodName: "open", arguments: ["playerId": id, "uri": "stub://texture"])
      ) { _ in opened.fulfill() }
      let released = expectation(description: "texture unregistered on main")
      plugin.handle(
        FlutterMethodCall(methodName: "releaseTexture", arguments: ["textureId": textureId])
      ) {
        value in
        XCTAssertTrue(Thread.isMainThread)
        XCTAssertNil(value)
        XCTAssertEqual(registry.unregistered, 1)
        released.fulfill()
      }
      XCTAssertEqual(registry.unregistered, 0)
      DispatchQueue.global().async { control.hold(0) }
      wait(for: [opened, released], timeout: 10)
      _ = call(plugin, "dispose", ["playerId": id])
    }
  #endif

  #if os(iOS)
    func testBackgroundPausesPlayAcceptedBehindSlowOpen() throws {
      let control = try stub()
      let plugin = plugin()
      let id = call(plugin, "create") as! Int64
      control.hold(1)
      defer { control.hold(0) }
      let opened = expectation(description: "background open")
      let played = expectation(description: "previously accepted play")
      plugin.handle(
        FlutterMethodCall(
          methodName: "open", arguments: ["playerId": id, "uri": "stub://background"])
      ) { _ in opened.fulfill() }
      plugin.handle(FlutterMethodCall(methodName: "play", arguments: ["playerId": id])) { value in
        XCTAssertFalse(value is FlutterError)
        played.fulfill()
      }
      plugin.enterBackgroundForTesting(playerId: id)
      DispatchQueue.global().async { control.hold(0) }
      wait(for: [opened, played], timeout: 10)
      XCTAssertEqual(
        call(plugin, "invokeJson", ["playerId": id, "method": "playing"]) as? String, "false")
      _ = call(plugin, "dispose", ["playerId": id])
    }
  #endif

  func testRetiredEngineSuppressesRepliesAndStillDestroys() throws {
    let control = try stub()
    var plugin: ErikaFlutterPlugin? = self.plugin()
    weak var released = plugin
    let id = call(plugin!, "create") as! Int64
    let startDestroyed = control.count(1)
    control.hold(1)
    defer { control.hold(0) }
    plugin!.handle(
      FlutterMethodCall(methodName: "open", arguments: ["playerId": id, "uri": "stub://teardown"])
    ) { _ in
      XCTFail("reply called after engine retirement")
    }
    plugin!.shutdownForTesting()
    plugin = nil
    XCTAssertNil(released, "native work must not retain the plugin/messenger")
    DispatchQueue.global().async { control.hold(0) }
    eventually { control.count(1) == startDestroyed + 1 }
  }

  func testEventsStayOnMainAndRetiredSubscriptionsDoNotFire() throws {
    _ = try stub()
    let plugin = plugin()
    let id = call(plugin, "create") as! Int64
    let event = expectation(description: "new subscription event")
    event.assertForOverFulfill = false
    _ = plugin.onListen(withArguments: nil) { _ in XCTFail("retired event sink") }
    _ = plugin.onCancel(withArguments: nil)
    _ = plugin.onListen(withArguments: nil) { _ in
      XCTAssertTrue(Thread.isMainThread)
      event.fulfill()
    }
    _ = call(plugin, "open", ["playerId": id, "uri": "stub://event"])
    wait(for: [event], timeout: 10)
    _ = plugin.onCancel(withArguments: nil)
    _ = call(plugin, "dispose", ["playerId": id])
  }
}
