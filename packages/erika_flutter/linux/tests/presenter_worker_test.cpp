#ifndef ERIKA_WORKER_HEADER
#define ERIKA_WORKER_HEADER "../presenter_worker.h"
#endif
#include ERIKA_WORKER_HEADER
#include <atomic>
#include <cstdlib>
#include <future>
#include <iostream>
#include <vector>

static void Require(bool value) { if (!value) std::abort(); }
using erika::PresenterWorker;
int main() {
  const auto ui = std::this_thread::get_id();
  PresenterWorker worker;
  std::thread::id owner;
  std::vector<int> order;
  std::promise<void> busy, release;
  auto released = release.get_future().share();
  worker.Post([&] { owner = std::this_thread::get_id(); order.push_back(1); busy.set_value(); released.wait(); });
  busy.get_future().wait();
  std::atomic<int> frames{0};
  std::promise<void> rendering, finish;
  auto finished = finish.get_future().share();
  Require(worker.RequestFrame([&] { Require(std::this_thread::get_id() == owner); ++frames; rendering.set_value(); finished.wait(); }));
  for (int i = 0; i < 10000; ++i) Require(!worker.RequestFrame([&] { ++frames; }));
  release.set_value(); rendering.get_future().wait();
  for (int i = 0; i < 10000; ++i) Require(!worker.RequestFrame([&] { ++frames; }));
  worker.Post([&] { Require(std::this_thread::get_id() == owner); order.push_back(2); });
  finish.set_value();
  worker.Stop([&] { Require(std::this_thread::get_id() == owner); order.push_back(3); });
  Require(owner != ui && frames == 1 && order == std::vector<int>({1, 2, 3}));
  Require(!worker.Post([] {}) && !worker.RequestFrame([] {}));
  worker.Stop();

  PresenterWorker stopping;
  std::promise<void> blocked, unblock;
  auto unblocked = unblock.get_future().share();
  stopping.Post([&] { blocked.set_value(); unblocked.wait(); }); blocked.get_future().wait();
  Require(stopping.RequestFrame([] { std::abort(); }));
  bool drained = false;
  stopping.Post([&] { drained = true; });
  std::thread stopper([&] { stopping.Stop([&] { Require(drained); }); });
  // Establish stop acceptance before releasing the blocked task, without sleeps.
  while (stopping.Post([] {})) std::this_thread::yield();
  unblock.set_value(); stopper.join();
  Require(drained);

  PresenterWorker clock;
  std::promise<void> first, resume;
  auto resumed = resume.get_future().share();
  std::atomic<int> ticks{0};
  clock.SetFrameTask([&] {
    Require(std::this_thread::get_id() != ui);
    if (++ticks == 1) { first.set_value(); resumed.wait(); }
  }, std::chrono::milliseconds(1));
  first.get_future().wait();
  // Miss many deadlines while a frame is in flight. A FIFO command must run
  // next, with no timer backlog. Disabling the clock fences future scheduling.
  std::this_thread::sleep_for(std::chrono::milliseconds(30));
  std::promise<void> command;
  clock.Post([&] { Require(ticks == 1); clock.SetFrameTask({}); command.set_value(); });
  resume.set_value(); command.get_future().wait();
  clock.Call([&] { Require(ticks == 1); });
  clock.Stop();
  std::cout << "ownership, FIFO, 20000 merged ticks, queued cancellation, drain/join, native clock skips missed deadlines passed\n";
}
