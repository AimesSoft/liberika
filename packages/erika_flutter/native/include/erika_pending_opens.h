// Copyright (c) Aimes Soft and contributors.
// SPDX-License-Identifier: MIT
#pragma once

#include "erika.h"
#include <algorithm>
#include <memory>
#include <vector>

// Linux and OpenHarmony can load a runtime released before cancellable JSON opens.
#pragma weak erika_presenter_open_json_cancellable
#pragma weak erika_cold_open_cancellation_create
#pragma weak erika_cold_open_cancellation_interrupt
#pragma weak erika_cold_open_cancellation_release

namespace erika {
// Only the platform thread touches the list. The queued native operation holds
// the ticket until it returns, including when it hasn't started at exit time.
class PendingJsonOpens {
 public:
  using Ticket = std::shared_ptr<ErikaColdOpenCancellation>;

  Ticket Prepare() {
    if (!erika_presenter_open_json_cancellable || !erika_cold_open_cancellation_create ||
        !erika_cold_open_cancellation_interrupt || !erika_cold_open_cancellation_release)
      return {};
    auto* handle = erika_cold_open_cancellation_create();
    if (!handle) return {};
    Ticket ticket(handle, erika_cold_open_cancellation_release);
    pending_.erase(std::remove_if(pending_.begin(), pending_.end(),
        [](const auto& open) { return open.expired(); }), pending_.end());
    pending_.push_back(ticket);
    return ticket;
  }

  void Interrupt() {
    for (const auto& open : pending_) {
      if (auto ticket = open.lock())
        erika_cold_open_cancellation_interrupt(ticket.get());
    }
    pending_.clear();
  }

 private:
  std::vector<std::weak_ptr<ErikaColdOpenCancellation>> pending_;
};
}  // namespace erika
