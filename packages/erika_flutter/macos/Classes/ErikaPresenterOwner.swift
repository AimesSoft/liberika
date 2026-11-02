import Foundation

/// A physical thread (rather than a GCD queue, which may migrate between
/// threads) owns the entire native presenter lifetime. No task waits for main.
/// Commands are FIFO. A pending display target replaces the previous target;
/// one frame runs between commands so a command burst cannot starve video.
final class ErikaPresenterOwner {
  private let condition = NSCondition()
  private var commands: [(label: String, task: () -> Void)] = []
  private var commandIndex = 0
  private var frame: (() -> Void)?
  private var maintenance: (() -> Void)?
  private var stopping = false
  private var finalizer: (() -> Void)?
  private var thread: Thread!
  private let traceEnabled = ProcessInfo.processInfo.environment["ERIKA_FLUTTER_FRAME_TRACE"] == "1"
  private let id: Int64

  init(id: Int64) {
    self.id = id
    thread = Thread { [weak self] in self?.run() }
    thread.name = "erika.presenter.\(id)"
    thread.qualityOfService = .userInteractive
    thread.start()
  }

  var isCurrent: Bool { Thread.current === thread }

  func assertCurrent() {
    precondition(isCurrent && !Thread.isMainThread, "Native presenter escaped its owner")
  }

  @discardableResult
  func post(_ label: String = "command", _ task: @escaping () -> Void) -> Bool {
    condition.lock()
    defer { condition.unlock() }
    guard !stopping else { return false }
    commands.append((label, task))
    condition.signal()
    return true
  }

  func requestFrame(_ task: @escaping () -> Void) {
    condition.lock()
    if !stopping {
      frame = task
      condition.signal()
    }
    condition.unlock()
  }

  /// Polls coalesce independently of video, including unattached players.
  func requestMaintenance(_ task: @escaping () -> Void) {
    condition.lock()
    if !stopping {
      maintenance = task
      condition.signal()
    }
    condition.unlock()
  }

  func cancelFrame() {
    condition.lock()
    frame = nil
    condition.unlock()
  }

  /// Nonblocking close: discard obsolete ticks, drain accepted commands, then
  /// destroy on this thread. The thread retains itself until destruction ends.
  func finish(_ task: @escaping () -> Void) {
    condition.lock()
    if !stopping {
      stopping = true
      frame = nil
      maintenance = nil
      finalizer = task
      condition.signal()
    }
    condition.unlock()
  }

  private func trace(_ kind: String) {
    if traceEnabled {
      NSLog(
        "[ErikaFrameTrace] player=%lld task=%@ thread=%@ main=%@",
        id, kind, String(describing: Thread.current), Thread.isMainThread.description)
    }
  }

  private func run() {
    while true {
      condition.lock()
      while commandIndex == commands.count && frame == nil && maintenance == nil && !stopping {
        condition.wait()
      }
      let command: (label: String, task: () -> Void)?
      if commandIndex < commands.count {
        command = commands[commandIndex]
        // Release captured hosts/results as they drain, even in a large burst.
        commands[commandIndex] = ("", {})
        commandIndex += 1
        if commandIndex == commands.count {
          commands.removeAll(keepingCapacity: true)
          commandIndex = 0
        }
      } else {
        command = nil
      }
      let done = stopping && command == nil
      let destroy = done ? finalizer : nil
      if done { finalizer = nil }
      condition.unlock()

      if done {
        autoreleasepool {
          trace("destroy")
          destroy?()
        }
        return
      }
      if let command {
        autoreleasepool {
          trace(command.label)
          command.task()
        }
      }

      // Take the latest target *after* the command; a slow open must not replay
      // the display target it received before entering native code.
      condition.lock()
      let tick = stopping ? nil : frame
      let poll = stopping ? nil : maintenance
      frame = nil
      maintenance = nil
      condition.unlock()
      if let tick {
        autoreleasepool {
          trace("tick")
          tick()
        }
      }
      if let poll {
        autoreleasepool {
          trace("poll")
          poll()
        }
      }
    }
  }
}

/// Cached scalar state only. The lock is never held across a native call or UI
/// operation, so platform readers cannot wait behind decoding/rendering.
@propertyWrapper
final class ErikaSnapshot<Value> {
  private let lock = NSLock()
  private var value: Value
  init(wrappedValue: Value) { value = wrappedValue }
  var wrappedValue: Value {
    get {
      lock.lock()
      defer { lock.unlock() }
      return value
    }
    set {
      lock.lock()
      value = newValue
      lock.unlock()
    }
  }
}

/// A surface can move between presenters without main waiting for either
/// owner. The new owner waits only for the old owner's native detach/destroy.
/// Every dependency refers to an earlier main-thread attachment, so ownership
/// transfers cannot form a cycle. Completion is idempotent for failed attaches.
final class ErikaSurfaceLease {
  weak var owner: AnyObject?
  private let group = DispatchGroup()
  private let lock = NSLock()
  private var released = false

  init(owner: AnyObject) {
    self.owner = owner
    group.enter()
  }

  func wait() {
    precondition(!Thread.isMainThread)
    group.wait()
  }

  func complete() {
    lock.lock()
    if !released {
      released = true
      group.leave()
    }
    lock.unlock()
  }
}

/// Main-thread completion slots preserve accepted method-result order even
/// when UI-only commands finish ahead of native work. Invalidation drops every
/// retained Flutter callback before the engine's messenger can disappear.
final class ErikaPlatformDelivery {
  private final class Stream {
    var next = 0
    var delivered = 0
    var slots: [Int: () -> Void] = [:]
  }
  private var streams: [Int64: Stream] = [:]
  private(set) var valid = true

  func reserve(key: Int64 = 0, _ callback: @escaping (Any?) -> Void) -> (Any?) -> Void {
    precondition(Thread.isMainThread)
    let stream = streams[key] ?? Stream()
    streams[key] = stream
    let sequence = stream.next
    stream.next += 1
    return { [weak self] value in
      let deliver = { [weak self] in
        guard let self, self.valid else { return }
        stream.slots[sequence] = { callback(value) }
        while let ready = stream.slots.removeValue(forKey: stream.delivered) {
          stream.delivered += 1
          ready()
        }
      }
      if Thread.isMainThread { deliver() } else { DispatchQueue.main.async(execute: deliver) }
    }
  }

  func invalidate() {
    precondition(Thread.isMainThread)
    valid = false
    streams.values.forEach { $0.slots.removeAll() }
    streams.removeAll()
  }
}
