import Foundation

enum ErikaTickRequest: Equatable {
  case render(presentationTime: Double?)
  case audioOnly
}

/// One render worker, at most one pending tick. A delayed worker consumes the
/// newest display target instead of dropping it or replaying a queue of frames.
final class ErikaTickMailbox {
  private let lock = NSLock()
  private var pending: ErikaTickRequest?
  private var workerScheduled = false
  private var lastDisplayTarget: Double?

  /// True means the caller must dispatch a worker; false reuses the active one.
  func submit(_ request: ErikaTickRequest) -> Bool {
    lock.lock()
    defer { lock.unlock() }
    if case let .render(target?) = request {
      guard target.isFinite else { return false }
      if let last = lastDisplayTarget, target <= last { return false }
      lastDisplayTarget = target
    }
    pending = request
    guard !workerScheduled else { return false }
    workerScheduled = true
    return true
  }

  func take() -> ErikaTickRequest? {
    lock.lock()
    defer { lock.unlock() }
    let request = pending
    pending = nil
    if request == nil { workerScheduled = false }
    return request
  }

  /// Driver changes discard queued work, without starting a second worker.
  func cancelPending() {
    lock.lock()
    pending = nil
    lastDisplayTarget = nil
    lock.unlock()
  }
}
