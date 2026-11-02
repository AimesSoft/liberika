#ifndef FLUTTER_PLUGIN_ERIKA_FLUTTER_PLUGIN_H_
#define FLUTTER_PLUGIN_ERIKA_FLUTTER_PLUGIN_H_

#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>

#include <flutter/encodable_value.h>
#include <flutter/event_channel.h>
#include <flutter/event_sink.h>
#include <flutter/event_stream_handler.h>
#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>

#include <atomic>
#include <cstdint>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <unordered_map>

#include "erika.h"
#include "erika_windows_smtc.h"

namespace erika_flutter {

class ErikaFlutterPlugin;

class ErikaEventStreamHandler
    : public flutter::StreamHandler<flutter::EncodableValue> {
 public:
  explicit ErikaEventStreamHandler(ErikaFlutterPlugin* plugin);

 protected:
  std::unique_ptr<flutter::StreamHandlerError<flutter::EncodableValue>>
  OnListenInternal(
      const flutter::EncodableValue* arguments,
      std::unique_ptr<flutter::EventSink<flutter::EncodableValue>>&& events)
      override;

  std::unique_ptr<flutter::StreamHandlerError<flutter::EncodableValue>>
  OnCancelInternal(const flutter::EncodableValue* arguments) override;

 private:
  ErikaFlutterPlugin* plugin_;
};

class ErikaFlutterPlugin : public flutter::Plugin {
 public:
  static void RegisterWithRegistrar(flutter::PluginRegistrarWindows* registrar,
                                   FlutterDesktopMessengerRef messenger);

  ErikaFlutterPlugin(flutter::PluginRegistrarWindows* registrar,
                     FlutterDesktopMessengerRef messenger);
  ~ErikaFlutterPlugin() override;

  ErikaFlutterPlugin(const ErikaFlutterPlugin&) = delete;
  ErikaFlutterPlugin& operator=(const ErikaFlutterPlugin&) = delete;

  void HandleMethodCall(
      const flutter::MethodCall<flutter::EncodableValue>& method_call,
      std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result);

  void SetEventSink(
      std::unique_ptr<flutter::EventSink<flutter::EncodableValue>> sink);
  void ClearEventSink();

 private:
  struct ErikaNativeLibrary;
  struct ErikaOverlayWindow;
  struct ErikaFlutterTexture;
  struct PlayerHost;
  using PlayerMap = std::unordered_map<int64_t, std::unique_ptr<PlayerHost>>;

  friend class ErikaEventStreamHandler;
  friend class ErikaFlutterPluginTestPeer;

  HWND FlutterWindow() const;
  double BackingScale() const;
  void StartFrameTimer();
  void StopFrameTimer();
  void PostFrameTick(HWND hwnd, uint64_t generation);
  void FrameTimerThreadMain(HWND hwnd,
                            HANDLE stop_event,
                            double interval_ms,
                            uint64_t generation);
  HWND EnsureFrameMessageWindow();
  void DestroyFrameMessageWindow();
  void RefreshFrameTimerForCurrentDisplay();
  static LRESULT CALLBACK FrameMessageWindowProc(HWND hwnd,
                                                 UINT message,
                                                 WPARAM wparam,
                                                 LPARAM lparam);
  void OnFrameTimer();
  void PostCompletion(std::function<void()> completion);
  void DrainCompletions();
  void DispatchNativeMethod(PlayerHost& host, std::string method,
                            flutter::EncodableMap args,
                            std::shared_ptr<flutter::MethodResult<flutter::EncodableValue>> result);
  std::optional<LRESULT> OnTopLevelWindowProc(HWND hwnd,
                                              UINT message,
                                              WPARAM wparam,
                                              LPARAM lparam);

  ErikaOverlayWindow& EnsureOverlayWindow();
  static void RecreateOverlayWindow(
      const PlayerMap& players,
      std::unique_ptr<ErikaOverlayWindow>& overlay,
      HWND parent);
  static void AttachOverlayPlayer(const PlayerMap& players,
                                  PlayerHost& host,
                                  ErikaOverlayWindow& overlay);
  static bool DetachOverlayPlayer(PlayerHost& host,
                                  ErikaOverlayWindow* overlay,
                                  std::optional<int64_t> generation);
  HWND RequestedOverlayFlutterWindow() const;
  void UpdateOverlayTarget(const flutter::EncodableMap& args);
  PlayerHost& PlayerFromArgs(const flutter::EncodableMap& args);
  ErikaFlutterTexture& TextureFromArgs(const flutter::EncodableMap& args);
  int64_t CreateTexture(const flutter::EncodableMap& args);
  void ReleaseTexture(int64_t texture_id);
  void ResizeAttachedOverlay();
  int64_t CreatePlayer(const flutter::EncodableValue* arguments);
  void RemovePlayer(int64_t player_id);
  void SendEvent(flutter::EncodableValue event);
  void EnsureSmtc();
  void SetActivePlayer(int64_t player_id);
  void RefreshSmtc();
  void HandleSmtcCommand(ErikaSmtcCommand command, uint64_t position_micros);

  flutter::PluginRegistrarWindows* registrar_ = nullptr;
  // Retained so availability can be checked after the engine invalidates it.
  FlutterDesktopMessengerRef native_messenger_ = nullptr;
  std::unique_ptr<flutter::EventChannel<flutter::EncodableValue>>
      event_channel_;
  std::unique_ptr<flutter::EventSink<flutter::EncodableValue>> event_sink_;
  PlayerMap players_;
  std::unordered_map<int64_t, std::shared_ptr<ErikaFlutterTexture>> textures_;
  std::unique_ptr<ErikaOverlayWindow> overlay_window_;
  std::unique_ptr<ErikaWindowsSmtc> smtc_;
  int64_t active_player_id_ = 0;
  int64_t requested_flutter_view_id_ = 0;
  bool overlay_uses_secondary_window_ = false;
  int64_t next_player_id_ = 1;
  int window_proc_delegate_id_ = 0;
  HWND frame_message_window_ = nullptr;
  HANDLE frame_timer_stop_event_ = nullptr;
  std::thread frame_timer_thread_;
  std::atomic<bool> frame_timer_running_{false};
  std::atomic<bool> frame_tick_pending_{false};
  std::atomic<uint64_t> frame_timer_generation_{0};
  double frame_timer_target_fps_ = 0.0;
  double frame_timer_interval_ms_ = 0.0;
  bool joining_presenter_ = false;
  std::mutex completion_mutex_;
  std::deque<std::function<void()>> completions_;
  bool completion_message_pending_ = false;
  uint64_t event_sink_generation_ = 0;
};

}  // namespace erika_flutter

#endif  // FLUTTER_PLUGIN_ERIKA_FLUTTER_PLUGIN_H_
