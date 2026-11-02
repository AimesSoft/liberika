#include "../erika_presenter_worker.h"

#include <atomic>
#include <cstdlib>
#include <future>
#include <iostream>
#include <vector>

using erika_flutter::ErikaPresenterWorker;
static void Require(bool value) { if (!value) std::abort(); }

int main() {
  const auto platform = std::this_thread::get_id();
  ErikaPresenterWorker worker;
  std::thread::id owner;
  std::vector<int> order;
  std::promise<void> entered, release;
  auto released = release.get_future().share();
  worker.Post([&] {
    owner = std::this_thread::get_id();
    order.push_back(1);  // Create.
    entered.set_value();
    released.wait();
  });
  entered.get_future().wait();
  std::atomic<int> frames{0};
  std::promise<void> rendering, finish_render;
  auto finish = finish_render.get_future().share();
  Require(worker.RequestFrame([&] {
    Require(std::this_thread::get_id() == owner);
    ++frames;
    rendering.set_value();
    finish.wait();
  }));
  for (int i = 0; i < 10000; ++i)
    Require(!worker.RequestFrame([&] { ++frames; }));
  release.set_value();
  rendering.get_future().wait();
  for (int i = 0; i < 10000; ++i)
    Require(!worker.RequestFrame([&] { ++frames; }));
  worker.Post([&] {
    Require(std::this_thread::get_id() == owner);
    order.push_back(2);  // Seek/pause after the slow frame.
  });
  finish_render.set_value();
  worker.Stop([&] {
    Require(std::this_thread::get_id() == owner);
    order.push_back(3);  // Destroy on the creating thread.
  });
  Require(owner != platform && frames == 1);
  Require(order == std::vector<int>({1, 2, 3}));
  Require(!worker.Post([] {}) && !worker.RequestFrame([] {}));
  worker.Stop();  // Idempotent join.

  ErikaPresenterWorker stopping;
  std::promise<void> busy, unblock;
  auto unblocked = unblock.get_future().share();
  stopping.Post([&] { busy.set_value(); unblocked.wait(); });
  busy.get_future().wait();
  Require(stopping.RequestFrame([] { std::abort(); }));
  bool command_drained = false;
  stopping.Post([&] { command_drained = true; });
  std::thread::id destroyed;
  stopping.Stop([&] { destroyed = std::this_thread::get_id(); },
                [&](std::thread&) { unblock.set_value(); });
  Require(command_drained && destroyed != platform);
  std::cout << "worker ownership, FIFO, 20000 coalesced requests, queued-tick cancellation, command drain and join passed\n";
}
