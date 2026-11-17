import CoreVideo
import QuartzCore

/// Map CoreVideo's monotonic host timestamp to the clock used by the Swift
/// presenter. Carry this absolute target through the owner mailbox, then compute
/// the relative delay immediately before entering the C API.
func erikaDisplayPresentationTime(
  _ timestamp: CVTimeStamp,
  hostTime: UInt64 = CVGetCurrentHostTime(),
  hostFrequency: Double = CVGetHostClockFrequency(),
  now: Double = CACurrentMediaTime()
) -> Double? {
  guard timestamp.flags & CVTimeStampFlags.hostTimeValid.rawValue != 0,
        hostFrequency.isFinite, hostFrequency > 0 else {
    return nil
  }
  // Subtract integers before converting to preserve precision at long uptimes,
  // and handle late targets without unsigned underflow.
  let delta = timestamp.hostTime >= hostTime
    ? Double(timestamp.hostTime - hostTime) / hostFrequency
    : -Double(hostTime - timestamp.hostTime) / hostFrequency
  let target = now + delta
  return target.isFinite ? target : nil
}
