import Foundation

// Compile this with ios/Classes/ErikaTickMailbox.swift for iphonesimulator and
// run it with simctl spawn. It exercises the real mailbox without a UI runner.
let mailbox = ErikaTickMailbox()
assert(mailbox.submit(.render(presentationTime: 1.0)))
assert(!mailbox.submit(.render(presentationTime: 1.016)))
assert(!mailbox.submit(.render(presentationTime: 1.032)))
assert(mailbox.take() == .render(presentationTime: 1.032))

// Requests arriving during a render reuse the worker and retain the latest.
assert(!mailbox.submit(.render(presentationTime: 1.048)))
assert(!mailbox.submit(.render(presentationTime: 1.064)))
assert(mailbox.take() == .render(presentationTime: 1.064))
assert(mailbox.take() == nil)
assert(!mailbox.submit(.render(presentationTime: 1.064)))
assert(!mailbox.submit(.render(presentationTime: 1.032)))
assert(!mailbox.submit(.render(presentationTime: .nan)))
assert(mailbox.submit(.render(presentationTime: 1.080)))

// Detach/background discards pending display work; audio can take its place.
mailbox.cancelPending()
assert(!mailbox.submit(.audioOnly))
assert(mailbox.take() == .audioOnly)
assert(mailbox.take() == nil)
assert(mailbox.submit(.render(presentationTime: nil)))
assert(mailbox.take() == .render(presentationTime: nil))
assert(mailbox.take() == nil)

// The new display driver's time origin is allowed to restart.
assert(mailbox.submit(.render(presentationTime: 0.1)))
assert(mailbox.take() == .render(presentationTime: 0.1))
assert(mailbox.take() == nil)
print("PASS: iOS tick coalescing, duplicate/stale rejection, driver changes")
