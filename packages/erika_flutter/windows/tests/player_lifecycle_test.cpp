// Include the implementation so the test peer exercises the actual internal
// PlayerHost and native-library loader, without starting a Flutter/DWM window.
#include "../erika_flutter_plugin.cpp"

#include <cstdlib>
#include <iostream>

namespace erika_flutter {

class ErikaFlutterPluginTestPeer {
 public:
  static void Require(bool value) { if (!value) std::abort(); }
  inline static const DWORD platform_thread = GetCurrentThreadId();
  inline static std::mutex completion_mutex;
  inline static std::deque<std::function<void()>> completions;
  static void Post(std::function<void()> completion) {
    std::lock_guard<std::mutex> lock(completion_mutex);
    completions.push_back(std::move(completion));
  }
  static void Drain() {
    Require(GetCurrentThreadId() == platform_thread);
    std::deque<std::function<void()>> ready;
    { std::lock_guard<std::mutex> lock(completion_mutex); ready.swap(completions); }
    for (auto& completion : ready) completion();
  }
  static void WaitNative(ErikaFlutterPlugin::PlayerHost& player) {
    const auto fence = player.Fence();
    while (fence.wait_for(std::chrono::milliseconds(0)) != std::future_status::ready) {
      MsgWaitForMultipleObjectsEx(0, nullptr, 10, QS_SENDMESSAGE, MWMO_INPUTAVAILABLE);
      MSG message;
      PeekMessageW(&message, nullptr, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
    }
    fence.get();
  }
  static void Wait(ErikaFlutterPlugin::PlayerHost& player) { WaitNative(player); Drain(); }
  static void Flush(const ErikaFlutterPlugin::PlayerMap& players) {
    for (const auto& entry : players) Wait(*entry.second);
    Drain();
  }
  static void BindFrames(ErikaFlutterPlugin::PlayerHost& player) {
    player.publish_frame = [&player](const auto& output) { player.ApplyFrame(output); };
  }

  static void Command(ErikaFlutterPlugin::PlayerHost& player, std::function<void()> command) {
    player.Submit([command] { command(); return EncodableValue(); },
                 [](EncodableValue, std::string error) {
      Require(GetCurrentThreadId() == platform_thread);
      if (!error.empty()) std::cerr << error << "\n";
      Require(error.empty());
    });
    Wait(player);
  }

  static void RunScreenshotEncoding(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library) {
    const auto capture = library->capture_frame;
    library->capture_frame = [](ErikaPresenterHandle*, uint32_t width,
        uint32_t height, uint8_t* bytes, uintptr_t capacity) {
      Require(GetCurrentThreadId() != platform_thread);
      Require(capacity == static_cast<uintptr_t>(width) * height * 4);
      std::fill(bytes, bytes + capacity, static_cast<uint8_t>(255));
      return ErikaStatus_Ok;
    };
    {
      ErikaFlutterPlugin::PlayerHost player(72, library, ErikaPresenterConfig{}, Post);
      player.Submit([&] {
        return player.Screenshot(EncodableMap{
            {EncodableValue("width"), EncodableValue(16)},
            {EncodableValue("height"), EncodableValue(8)}});
      }, [](EncodableValue value, std::string error) {
        if (!error.empty()) std::cerr << "PNG capture: " << error << "\n";
        Require(GetCurrentThreadId() == platform_thread && error.empty());
        const auto* png = std::get_if<std::vector<uint8_t>>(&value);
        Require(png && png->size() > 24 && (*png)[0] == 0x89 && (*png)[1] == 'P');
        Require((*png)[19] == 16 && (*png)[23] == 8);
      });
      Wait(player);
    }
    Drain();
    library->capture_frame = capture;
    std::cout << "worker capture (stub RGBA), real WIC PNG encoding and platform result passed\n";
  }

  static void RunPlaybackSmoke(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library,
      const std::string& clip) {
    if (clip.empty()) return;
    Registrar registrar;
    ErikaFlutterPlugin::ErikaFlutterTexture texture(&registrar, 320, 180, 1.0);
    {
      ErikaFlutterPlugin::PlayerHost player(71, library, ErikaPresenterConfig{}, Post);
      BindFrames(player);
      Wait(player);
      player.AttachFlutterTexture(texture);
      Wait(player);
      Require(player.surface_error.empty());
      Command(player, [&] { player.Open(clip, EncodableMap{}); player.Play(); });
      bool rendered = false;
      for (int i = 0; i < 200 && !rendered; ++i) {
        player.RequestTick(0); Wait(player);
        player.Submit([&] {
          return EncodableValue(player.latest_presenter_stats.rendered_video_frames > 0);
        }, [&](EncodableValue value, std::string error) {
          Require(error.empty());
          rendered = std::get<bool>(value);
        });
        Wait(player);
        if (!rendered) std::this_thread::sleep_for(std::chrono::milliseconds(20));
      }
      Require(rendered);
      player.Submit([&] {
          return player.Screenshot(EncodableMap{
              {EncodableValue("width"), EncodableValue(320)},
              {EncodableValue("height"), EncodableValue(180)}});
        }, [&](EncodableValue value, std::string error) {
          Require(GetCurrentThreadId() == platform_thread);
          Require(error.empty());
          const auto* bytes = std::get_if<std::vector<uint8_t>>(&value);
          if (bytes) {
            Require(bytes->size() > 8 && (*bytes)[0] == 0x89 && (*bytes)[1] == 'P');
            std::cout << "runtime PNG capture passed\n";
          } else {
            Require(std::holds_alternative<std::monostate>(value));
            std::cout << "runtime screenshot returned null (D3D11 capture unavailable)\n";
          }
        });
        Wait(player);
      Command(player, [&] { player.Pause(); player.Seek(1000000); });
      player.RequestTick(0); Wait(player);
      Command(player, [&] { player.Play(); });
      player.ResizeFlutterTexture(texture, 640, 360, 1.0);
      player.RequestTick(0); Wait(player);
      Require(player.surface_error.empty());
      player.Detach(std::nullopt);
      Wait(player);
      player.AttachFlutterTexture(texture);
      player.RequestTick(0); Wait(player);
      Require(player.surface_error.empty());
      Command(player, [&] { player.Pause(); });
    }
    Drain();
    Require(texture.owner_player_id == 0);
    std::cout << "4K HEVC runtime open/play/render, pause/seek/resume, resize, detach/reattach and joined disposal passed\n";
  }

  // Opt-in desktop smoke: exercises the same HWND/Composition attachment and
  // platform publication as ErikaVideoView, rather than a Flutter texture.
  static void RunNativeSurfaceSmoke(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library,
      const std::string& clip) {
    HWND window = CreateWindowExW(0, L"STATIC", L"Erika surface smoke",
        WS_OVERLAPPEDWINDOW, 0, 0, 640, 480, nullptr, nullptr,
        GetModuleHandleW(nullptr), nullptr);
    Require(window != nullptr);
    ShowWindow(window, SW_SHOWNOACTIVATE);
    using Diagnostics = ErikaStatus (*)(ErikaPresenterHandle*, char**);
    const auto diagnostics = reinterpret_cast<Diagnostics>(GetProcAddress(
        library->module, "erika_presenter_get_diagnostics_json"));
    for (bool composition : {false, true}) {
      ErikaFlutterPlugin::ErikaOverlayWindow overlay(window);
      overlay.ConfigureComposition(composition ? "overlay" : "srcOver", 1.0);
      {
        ErikaFlutterPlugin::PlayerHost player(81, library, ErikaPresenterConfig{}, Post);
        BindFrames(player);
        Wait(player);
        overlay.owner_player_id = player.id;
        player.AttachOverlay(overlay);
        overlay.SetFrame(0, 0, 320, 180, true, std::nullopt, std::nullopt);
        player.ResizeOverlay(overlay);
        Wait(player);
        Require(player.surface_error.empty());
        Command(player, [&] { player.Open(clip, EncodableMap{}); player.Play(); });
        for (int i = 0; i < 150; ++i) {
          player.RequestTick(0); Wait(player);
          MSG message;
          while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message); DispatchMessageW(&message);
          }
          std::this_thread::sleep_for(std::chrono::milliseconds(20));
        }
        Command(player, [&] {
          const auto& stats = player.latest_presenter_stats;
          Require(stats.rendered_video_frames > 0 && stats.render_failures == 0);
          Require(player.native_surface_attached);
          if (diagnostics) {
            char* json = nullptr;
            player.CheckNative(diagnostics(player.handle, &json), "diagnostics");
            std::cout << (composition ? "COMPOSITION " : "HWND ") << json << "\n";
            library->string_free(json);
          }
          player.Pause();
          if (composition) {
            void* raw = nullptr;
            player.CheckNative(library->windows_composition_swapchain(player.handle, &raw),
                               "composition swapchain readback");
            Microsoft::WRL::ComPtr<IUnknown> content;
            content.Attach(static_cast<IUnknown*>(raw));
            Microsoft::WRL::ComPtr<IDXGISwapChain> swapchain;
            Require(SUCCEEDED(content.As(&swapchain)));
            Microsoft::WRL::ComPtr<ID3D11Texture2D> buffer;
            Require(SUCCEEDED(swapchain->GetBuffer(0, IID_PPV_ARGS(&buffer))));
            Microsoft::WRL::ComPtr<ID3D11Device> device;
            buffer->GetDevice(&device);
            Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
            device->GetImmediateContext(&context);
            D3D11_TEXTURE2D_DESC desc{};
            buffer->GetDesc(&desc);
            Require(desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM ||
                    desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM ||
                    desc.Format == DXGI_FORMAT_R10G10B10A2_UNORM);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = desc.MiscFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
            Require(SUCCEEDED(device->CreateTexture2D(&desc, nullptr, &staging)));
            context->CopyResource(staging.Get(), buffer.Get());
            D3D11_MAPPED_SUBRESOURCE mapped{};
            Require(SUCCEEDED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped)));
            const uint32_t first = *static_cast<const uint32_t*>(mapped.pData);
            size_t different = 0;
            for (UINT y = 0; y < desc.Height; ++y) {
              const auto* row = reinterpret_cast<const uint32_t*>(
                  static_cast<const uint8_t*>(mapped.pData) + y * mapped.RowPitch);
              for (UINT x = 0; x < desc.Width; ++x) different += row[x] != first;
            }
            context->Unmap(staging.Get(), 0);
            Require(different > 0);
            std::cout << "COMPOSITION nonuniform video pixels=" << different << "\n";
          }
        });
        if (composition) {
          Require(overlay.composition_content && overlay.composition_surface);
          Require(overlay.composition_target.Root() != nullptr);
          Microsoft::WRL::ComPtr<IDXGISwapChain> swapchain;
          Require(SUCCEEDED(overlay.composition_content.As(&swapchain)));
          UINT presents = 0;
          Require(SUCCEEDED(swapchain->GetLastPresentCount(&presents)));
          Require(presents > 0);
          std::cout << "COMPOSITION bound root, swapchain presents=" << presents << "\n";
        } else {
          Require(GetWindow(window, GW_HWNDPREV) == overlay.hwnd);
          std::cout << "HWND overlay above host passed\n";
        }
        player.ResizeSurface(480, 270, 1.0);
        player.RequestTick(0); Wait(player);
        Require(player.surface_error.empty());
        player.Detach(std::nullopt); Wait(player);
        Require(!overlay.composition_content);
        if (composition) Require(overlay.composition_target.Root() == nullptr);
        player.AttachOverlay(overlay);
        player.RequestTick(0); Wait(player);
        Require(player.surface_error.empty());
        if (composition) Require(overlay.composition_target.Root() != nullptr);
      }
      Drain();
    }
    DestroyWindow(window);
    std::cout << "native HWND and Composition render, resize, detach/reattach and disposal passed\n";
  }

  class Registrar : public flutter::TextureRegistrar {
   public:
    int marked_frames = 0;
    int64_t RegisterTexture(flutter::TextureVariant*) override { return 13; }
    bool MarkTextureFrameAvailable(int64_t) override {
      Require(GetCurrentThreadId() == platform_thread);
      ++marked_frames;
      return true;
    }
    void UnregisterTexture(int64_t, std::function<void()> callback) override {
      if (callback) callback();
    }
    bool UnregisterTexture(int64_t) override { return true; }
  };

  inline static std::vector<ErikaPresenterHandle*> hwnd_attachments;
  inline static bool fail_hwnd_attach = false;
  inline static std::mutex owner_mutex;
  inline static std::unordered_map<ErikaPresenterHandle*, DWORD> native_owners;
  inline static ErikaFlutterPlugin::ErikaNativeLibrary::CreateWithConfigFn real_create;
  inline static ErikaFlutterPlugin::ErikaNativeLibrary::DestroyFn real_destroy;
  inline static ErikaFlutterPlugin::ErikaNativeLibrary::RenderTickFn real_render;

  static void TrackOwners(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library) {
    real_create = library->create_with_config;
    real_destroy = library->destroy;
    real_render = library->render_tick;
    Require(real_create != nullptr);
    library->create_with_config = [](ErikaPresenterConfig config) {
      Require(GetCurrentThreadId() != platform_thread);
      auto* handle = real_create(config);
      if (handle) {
        std::lock_guard<std::mutex> lock(owner_mutex);
        native_owners[handle] = GetCurrentThreadId();
      }
      return handle;
    };
    library->render_tick = [](ErikaPresenterHandle* handle, double time, ErikaPresenterStats* stats) {
      {
        std::lock_guard<std::mutex> lock(owner_mutex);
        Require(native_owners.at(handle) == GetCurrentThreadId());
      }
      return real_render(handle, time, stats);
    };
    library->destroy = [](ErikaPresenterHandle* handle) {
      {
        std::lock_guard<std::mutex> lock(owner_mutex);
        Require(native_owners.at(handle) == GetCurrentThreadId());
        native_owners.erase(handle);
      }
      real_destroy(handle);
    };
  }

  static void RunSentMessageJoin(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library) {
    HWND hwnd = CreateWindowExW(0, L"STATIC", L"worker join test", WS_POPUP,
        0, 0, 1, 1, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
    Require(hwnd != nullptr);
    std::atomic<bool> returned{false};
    std::promise<void> sending;
    {
      ErikaFlutterPlugin::PlayerHost player(73, library, ErikaPresenterConfig{}, Post);
      player.worker.Post([&] {
        sending.set_value();
        // Models DXGI calling an HWND owner while that owner is disposing.
        SendMessageW(hwnd, WM_GETTEXTLENGTH, 0, 0);
        returned = true;
      });
      sending.get_future().wait();
      // No message loop here. The destructor's sent-message waiter must allow
      // the worker to finish without dispatching posted platform completions.
    }
    Require(returned.load());
    Drain();
    DestroyWindow(hwnd);
    std::cout << "dispose services worker sent messages without a platform callback deadlock passed\n";
  }

  static void RunOverlayOwnership(
      const std::shared_ptr<ErikaFlutterPlugin::ErikaNativeLibrary>& library) {
    const auto attach = library->attach_windows_hwnd;
    const auto detach = library->detach_surface;
    library->attach_windows_hwnd = [](ErikaPresenterHandle* player,
        uint64_t, uint64_t, uint32_t, uint32_t, double) {
      Require(GetCurrentThreadId() != platform_thread);
      hwnd_attachments.push_back(player);
      return fail_hwnd_attach ? ErikaStatus_PlayerError : ErikaStatus_Ok;
    };
    library->detach_surface = [](ErikaPresenterHandle*) { return ErikaStatus_Ok; };
    // Hidden Win32 parents are sufficient to test actual overlay ownership
    // logic. Only the native surface attachment is stubbed, not the owner map.
    struct HiddenWindow {
      HWND hwnd = CreateWindowExW(0, L"STATIC", L"Erika ownership test",
          WS_POPUP, 0, 0, 1, 1, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
      ~HiddenWindow() { if (hwnd) DestroyWindow(hwnd); }
    };
    {
      HiddenWindow first_parent, second_parent;
      Require(first_parent.hwnd && second_parent.hwnd);
      auto overlay = std::make_unique<ErikaFlutterPlugin::ErikaOverlayWindow>(
          first_parent.hwnd);
      // The transparent Flutter host must stay above the native video so
      // playback controls remain visible. Check repeated frame updates too.
      for (int i = 0; i < 2; ++i) {
        overlay->SetFrame(0, 0, 8, 8, true, std::nullopt, std::nullopt);
        Require(GetWindow(first_parent.hwnd, GW_HWNDNEXT) == overlay->hwnd);
        Require((GetWindowLongPtrW(overlay->hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST) == 0);
      }
      overlay->SetFrame(0, 0, 0, 0, false, std::nullopt, std::nullopt);
      std::cout << "video underlay stays below Flutter controls without topmost passed\n";
      ErikaFlutterPlugin::PlayerMap players;
      for (int64_t id : {11, 22, 33}) {
        players.emplace(id, std::make_unique<ErikaFlutterPlugin::PlayerHost>(
            id, library, ErikaPresenterConfig{}, Post));
      }
      Flush(players);
      auto& original = *players.begin()->second; // Deliberately not the last entry.
      for (const auto& entry : players) {
        entry.second->attached_view_id = kWindowOverlayViewId;
        entry.second->surface_attached = true;
        entry.second->attached_hwnd = overlay->hwnd;
      }
      overlay->owner_player_id = original.id;
      hwnd_attachments.clear();
      ErikaFlutterPlugin::RecreateOverlayWindow(players, overlay, second_parent.hwnd);
      Flush(players);
      Require(overlay->owner_player_id == original.id);
      Require(hwnd_attachments.size() == 1 && hwnd_attachments[0] == original.handle);
      Require(original.attached_hwnd == overlay->hwnd);
      ErikaFlutterPlugin::PlayerHost* replacement = nullptr;
      for (const auto& entry : players) {
        if (entry.second.get() != &original) {
          Require(!entry.second->surface_attached && entry.second->attached_view_id == 0);
          replacement = entry.second.get();
        }
      }
      Require(replacement != nullptr);
      ErikaFlutterPlugin::AttachOverlayPlayer(players, *replacement, *overlay);
      Flush(players);
      Require(!original.surface_attached && replacement->surface_attached);
      Require(overlay->owner_player_id == replacement->id);
      overlay->visible = true; // Test state without showing any window.
      overlay->active_generation = 77;
      Require(ErikaFlutterPlugin::DetachOverlayPlayer(original, overlay.get(), std::nullopt));
      Require(overlay->visible && overlay->owner_player_id == replacement->id);
      Require(!ErikaFlutterPlugin::DetachOverlayPlayer(*replacement, overlay.get(), 76));
      Require(replacement->surface_attached && overlay->visible);
      Require(ErikaFlutterPlugin::DetachOverlayPlayer(*replacement, overlay.get(), 77));
      Require(!replacement->surface_attached && !overlay->visible && overlay->owner_player_id == 0);

      // Hidden/unowned overlays must not acquire an arbitrary stale producer.
      original.attached_view_id = kWindowOverlayViewId;
      original.surface_attached = true;
      hwnd_attachments.clear();
      ErikaFlutterPlugin::RecreateOverlayWindow(players, overlay, first_parent.hwnd);
      Flush(players);
      Require(hwnd_attachments.empty() && overlay->owner_player_id == 0);
      Require(!original.surface_attached);
      overlay->owner_player_id = original.id; // Stale ID without an attachment.
      ErikaFlutterPlugin::RecreateOverlayWindow(players, overlay, second_parent.hwnd);
      Flush(players);
      Require(hwnd_attachments.empty() && overlay->owner_player_id == 0);

      // A failed takeover must not retain the previous player's ownership.
      ErikaFlutterPlugin::AttachOverlayPlayer(players, original, *overlay);
      Flush(players);
      fail_hwnd_attach = true;
      ErikaFlutterPlugin::AttachOverlayPlayer(players, *replacement, *overlay);
      Flush(players);
      fail_hwnd_attach = false;
      Require(!replacement->surface_error.empty() && !replacement->native_surface_attached);
      Require(!original.surface_attached);

      // Platform detach can finish its bookkeeping while rendering is blocked.
      // Recreating the overlay during that interval must retain the old HWND.
      ErikaFlutterPlugin::AttachOverlayPlayer(players, original, *overlay);
      Flush(players);
      const HWND retired_hwnd = overlay->hwnd;
      std::promise<void> busy, release;
      auto released = release.get_future().share();
      original.worker.Post([&] { busy.set_value(); released.wait(); });
      busy.get_future().wait();
      original.Detach(std::nullopt);
      overlay->owner_player_id = 0;
      ErikaFlutterPlugin::RecreateOverlayWindow(players, overlay, first_parent.hwnd);
      Require(IsWindow(retired_hwnd));
      ErikaFlutterPlugin::AttachOverlayPlayer(players, *replacement, *overlay);
      const auto takeover = replacement->Fence();
      Require(takeover.wait_for(std::chrono::milliseconds(10)) != std::future_status::ready);
      release.set_value();
      Flush(players);
      Require(!IsWindow(retired_hwnd));
      players.clear();
      Drain();

    }
    library->attach_windows_hwnd = attach;
    library->detach_surface = detach;
    std::cout << "overlay owner preservation, exclusive takeover and stale detach protection passed\n";
  }

  static void Run(const std::string& clip, bool native_surface = false) {
    auto library = ErikaFlutterPlugin::ErikaNativeLibrary::Shared();
    TrackOwners(library);
    RunSentMessageJoin(library);
    RunOverlayOwnership(library);
    RunScreenshotEncoding(library);
    RunPlaybackSmoke(library, clip);
    if (native_surface) RunNativeSurfaceSmoke(library, clip);
    ErikaPresenterConfig config{};
    Registrar registrar;
    ErikaFlutterPlugin::ErikaFlutterTexture texture(&registrar, 4, 4, 1.0);
    if (library->attach_flutter_texture && library->windows_flutter_texture) {
      // When run against a source-built DLL, exercise the actual bridge all
      // the way to D3D11 too; no replacement function pointers in this block.
      {
        ErikaFlutterPlugin::PlayerHost player(4, library, config, Post);
        Wait(player);
        BindFrames(player);
        player.AttachFlutterTexture(texture);
        player.RequestTick(0); Wait(player);
        Require(registrar.marked_frames == 1);
        const auto* first = texture.publication.AcquireDescriptor();
        Require(first && first->width == 4 && first->height == 4);
        for (int i = 0; i < 10; ++i) { player.RequestTick(0); Wait(player); }
        Require(registrar.marked_frames == 1);
        player.ResizeFlutterTexture(texture, 8, 6, 1.0);
        player.RequestTick(0); Wait(player);
        Require(registrar.marked_frames == 2);
        Require(first->width == 4 && first->height == 4);
        const auto* resized = texture.publication.AcquireDescriptor();
        Require(resized && resized->width == 8 && resized->height == 6);
        // A completed frame still waiting in the platform queue cannot undo
        // detach, or publish into a newly attached surface generation.
        player.RequestTick(0);
        WaitNative(player);
        player.Detach(std::nullopt);
        Drain();
        Require(texture.publication.AcquireDescriptor() == nullptr);
        player.AttachFlutterTexture(texture);
        player.RequestTick(0); Wait(player);
        Require(texture.publication.AcquireDescriptor() != nullptr);
      }
      Drain();
      Require(texture.owner_player_id == 0);
      Require(texture.publication.AcquireDescriptor() == nullptr);
      std::cout << "source runtime texture attach, idle, resize, stale completion, detach/reattach and disposal passed\n";
    }
    // Load the bundled runtime (including v0.1.7 without texture symbols).
    // Also exercise absence when a future bundled runtime provides them.
    library->attach_flutter_texture = nullptr;
    library->windows_flutter_texture = nullptr;
    {
      ErikaFlutterPlugin::PlayerHost player(1, library, config, Post);
      Wait(player);
      Require(player.handle != nullptr);
      player.surface_attached = true;
      bool rejected = false;
      try {
        player.AttachFlutterTexture(texture);
      } catch (const std::exception&) { rejected = true; }
      Require(rejected && player.surface_attached); // Failed opt-in is atomic.
      player.surface_attached = false;
    }

    // Supply only the new attachment capability; player creation/destruction
    // still use the real released DLL. This isolates the plugin ownership bug.
    library->attach_flutter_texture = [](ErikaPresenterHandle*, ErikaFlutterTextureKind,
        int64_t, uint32_t, uint32_t, double) { return ErikaStatus_Ok; };
    library->windows_flutter_texture = [](ErikaPresenterHandle*, void**) {
      return ErikaStatus_PlayerError;
    };
    {
      ErikaFlutterPlugin::PlayerHost player(2, library, config, Post);
      Wait(player);
      player.AttachFlutterTexture(texture);
      Wait(player);
      Require(texture.owner_player_id == 2);
    }
    Require(texture.owner_player_id == 0);
    {
      ErikaFlutterPlugin::PlayerHost replacement(3, library, config, Post);
      Wait(replacement);
      replacement.AttachFlutterTexture(texture);
      Wait(replacement);
      Require(texture.owner_player_id == 3);
    }
    Require(texture.owner_player_id == 0);
    Drain();
    Require(native_owners.empty());
    std::cout << "native create/render/destroy owner thread identities passed\n";
  }
};
}  // namespace erika_flutter

int wmain(int argc, wchar_t** argv) {
  if (argc != 2 && argc != 3 && argc != 4) return 1;
  const bool keep_loaded = argc == 3 && wcscmp(argv[2], L"--keep-runtime-loaded") == 0;
  if (argc == 3 && !keep_loaded) return 1;
  const bool native_surface = argc == 4 && wcscmp(argv[2], L"--surface-clip") == 0;
  if (argc == 4 && !native_surface && wcscmp(argv[2], L"--clip") != 0) return 1;
  if (native_surface) winrt::init_apartment(winrt::apartment_type::single_threaded);
  if (!SetEnvironmentVariableW(L"ERIKA_CAPI_DLL", argv[1])) return 1;
  // Compatibility-only fixture for released binaries with the pre-existing
  // unjoined danmaku worker. Source-runtime tests MUST exercise real unloading.
  if (keep_loaded && !LoadLibraryW(argv[1])) return 1;
  erika_flutter::ErikaFlutterPluginTestPeer::Run(
      argc == 4 ? erika_flutter::WideToUtf8(argv[3]) : std::string{}, native_surface);
  if (!keep_loaded) {
    erika_flutter::ErikaFlutterPluginTestPeer::Require(
        GetModuleHandleW(argv[1]) == nullptr);
    std::cout << "source runtime DLL unload passed\n";
  }
  std::cout << "runtime create/dispose, optional capability guard and texture owner cleanup passed\n";
}
