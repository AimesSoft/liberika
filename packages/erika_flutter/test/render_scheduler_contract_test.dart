import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

void main() {
  test('Windows texture APIs are optional for native runtime compatibility',
      () {
    final plugin = File('windows/erika_flutter_plugin.cpp').readAsStringSync();
    expect(plugin, contains('LoadOptional<AttachFlutterTextureFn>'));
    expect(plugin, contains('LoadOptional<GetWindowsFlutterTextureFn>'));
    expect(plugin, isNot(contains('LoadRequired<AttachFlutterTextureFn>')));
    expect(plugin, isNot(contains('LoadRequired<GetWindowsFlutterTextureFn>')));
  });

  test('new Windows texture export is declared in both SDK headers', () {
    const symbol = 'erika_presenter_windows_flutter_texture_iunknown';
    expect(File('native/include/erika.h').readAsStringSync(), contains(symbol));
    final publicHeader = File('../../crates/erika_capi/include/erika.h');
    if (publicHeader.existsSync()) {
      expect(publicHeader.readAsStringSync(), contains(symbol));
    }
  });

  test('macOS renders on the presenter owner, woken by CVDisplayLink', () {
    final plugin = File(
      'macos/Classes/ErikaFlutterPlugin.swift',
    ).readAsStringSync();

    final driverStart = plugin.indexOf(
      'private final class ErikaDisplayLinkDriver',
    );
    final driverEnd = plugin.indexOf('\n}\n', driverStart);
    expect(driverStart, greaterThanOrEqualTo(0));
    expect(driverEnd, greaterThan(driverStart));
    final driver = plugin.substring(driverStart, driverEnd);
    expect(driver, contains('CVDisplayLinkSetOutputCallback'));
    expect(driver, contains('onTick()'));
    expect(driver, isNot(contains('DispatchQueue')));

    // The display link only wakes the presenter's owner; ticks coalesce there.
    expect(plugin, contains('self.scheduleRenderTick()'));
    expect(plugin, contains('owner.requestFrame { [weak self] in'));
  });

  test('macOS event polling is coalescible and stops with the last player', () {
    final plugin = File(
      'macos/Classes/ErikaFlutterPlugin.swift',
    ).readAsStringSync();

    expect(plugin, contains('timer.tolerance = 0.01'));
    expect(plugin, contains('private func stopPollTimerIfIdle()'));
    expect(plugin, contains('players.removeValue(forKey: playerId)'));
    expect(plugin, contains('stopPollTimerIfIdle()'));
  });

  test('macOS Flutter texture path stays on IOSurface and Metal', () {
    final plugin = File(
      'macos/Classes/ErikaFlutterPlugin.swift',
    ).readAsStringSync();

    expect(plugin, contains('private final class ErikaFlutterTextureSurface'));
    expect(plugin, contains('kCVPixelBufferIOSurfacePropertiesKey'));
    expect(plugin, contains('CVMetalTextureCacheCreateTextureFromImage'));
    expect(plugin, contains('library.setFlutterTextureBuffer('));
    expect(plugin, contains('registry.textureFrameAvailable(id)'));
    expect(plugin, isNot(contains('CVPixelBufferLockBaseAddress')));
  });

  test(
    'macOS transparent platform view applies native overlay compositing',
    () {
      final plugin = File(
        'macos/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(plugin, contains('metalLayer.isOpaque = !alphaVideo'));
      expect(plugin, contains('NSColor.clear.cgColor'));
      expect(
        plugin,
        contains('metalLayer.compositingFilter = "overlayBlendMode"'),
      );
      expect(plugin, contains('metalLayer.opacity = Float('));
    },
  );

  test('Windows transparent video is composed into the Flutter HWND', () {
    final plugin = File('windows/erika_flutter_plugin.cpp').readAsStringSync();
    final blendEffect = File(
      'windows/erika_composition_blend_effect.h',
    ).readAsStringSync();
    final rendererFile = File('../../crates/erika/src/renderer/d3d11.rs');
    if (!rendererFile.existsSync()) {
      return;
    }
    final renderer = rendererFile.readAsStringSync();

    expect(plugin, contains('config.video_alpha_mode ='));
    expect(plugin, contains('capabilities.direct_composition = true'));
    expect(plugin, contains('QueryInterface(IDXGISwapChain)'));
    expect(plugin, contains('CreateDispatcherQueueController'));
    expect(plugin, contains('CreateDesktopWindowTarget'));
    expect(plugin, contains('CreateCompositionSurfaceForSwapChain'));
    expect(plugin, contains('CreateEffectFactory'));
    expect(plugin, contains('CreateBackdropBrush'));
    expect(plugin, contains('SetSourceParameter(\n        L"Backdrop"'));
    expect(plugin, contains('SetSourceParameter(L"Video"'));
    expect(plugin, contains('D2D1_BLEND_MODE_OVERLAY'));
    expect(blendEffect, contains('GetSourceCount(UINT* count)'));
    expect(blendEffect, contains('*count = 2'));
    expect(plugin, isNot(contains('DCompositionCreateDevice3')));
    expect(plugin, isNot(contains('CreateBlendEffect')));
    expect(plugin, isNot(contains('root_visual->SetEffect(effect)')));
    expect(
      plugin,
      contains('erika_presenter_windows_composition_swapchain_iunknown'),
    );
    expect(renderer, contains('DXGI_ALPHA_MODE_PREMULTIPLIED'));
    expect(
      renderer,
      contains('composition && self.video_alpha_mode.has_alpha()'),
    );
  });

  for (final entry in <(String, String)>[
    ('ios', 'scheduleTick(presentationTime: targetTimestamp)'),
    ('tvos', 'scheduleRenderTick()'),
  ]) {
    final platform = entry.$1;
    final schedulerCall = entry.$2;

    test('$platform keeps presenter rendering off the main run loop', () {
      final plugin = File(
        '$platform/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(plugin, contains('let owner: ErikaPresenterOwner'));
      expect(plugin, contains('owner.requestFrame { [weak self] in'));
      expect(plugin, isNot(contains('nativeCallLock')));
      expect(plugin, contains('self?.$schedulerCall'));
      expect(plugin, contains('mainThread=\\(Thread.isMainThread)'));
      expect(plugin, contains('Timer(timeInterval: 0.05'));
      expect(plugin, isNot(contains('self?.renderTick(sendEvent:')));

      final renderStart = plugin.indexOf(platform == 'ios'
          ? '  func renderTick(presentationTime:'
          : '  func renderTick() {');
      final pollStart = plugin.indexOf('  func pollEvents(', renderStart);
      expect(renderStart, greaterThanOrEqualTo(0));
      expect(pollStart, greaterThan(renderStart));
      expect(
        plugin.substring(renderStart, pollStart),
        isNot(contains('pollEvents(')),
      );
    });
  }

  test(
    'iOS retries audio-session activation after an allowed interruption',
    () {
      final plugin = File(
        'ios/Classes/ErikaFlutterPlugin.swift',
      ).readAsStringSync();

      expect(plugin, contains('AVAudioSession.interruptionNotification'));
      expect(plugin, contains('try session.setActive(true)'));
      expect(plugin, contains('private func resumeInterruptedPlayback'));
      expect(plugin, contains('interruptionResumeWorkItem'));
      expect(plugin, contains('guard attempt < maxAttempts else'));
    },
  );

  test('iOS drops a pending interruption resume on an explicit command', () {
    final plugin = File(
      'ios/Classes/ErikaFlutterPlugin.swift',
    ).readAsStringSync();

    expect(
      plugin,
      contains(
        'private func cancelPendingInterruptionResume(ifPlayer playerId: Int64)',
      ),
    );

    // Every explicit pause/stop/close path must disarm the retry before it
    // fires, or the deferred resume would play against the user's intent.
    for (final entry in <(String, String)>[
      ('      case "pause":', 'try host.pause()'),
      ('      case "stop":', 'try host.stop()'),
      ('      case "close":', 'try host.close()'),
    ]) {
      final caseStart = plugin.indexOf(entry.$1);
      expect(caseStart, greaterThanOrEqualTo(0), reason: entry.$1);
      final body = plugin.substring(
        caseStart,
        plugin.indexOf(entry.$2, caseStart) + entry.$2.length,
      );
      expect(
        body,
        contains('cancelPendingInterruptionResume(ifPlayer: host.id)'),
      );
    }

    final remoteStart = plugin.indexOf('private func performRemotePause()');
    final remoteEnd = plugin.indexOf(
      'private func performRemoteToggle()',
      remoteStart,
    );
    expect(remoteStart, greaterThanOrEqualTo(0));
    expect(remoteEnd, greaterThan(remoteStart));
    expect(
      plugin.substring(remoteStart, remoteEnd),
      contains('cancelPendingInterruptionResume(ifPlayer: host.id)'),
    );
  });

  test(
    'Android polls events on the presenter thread with adaptive scheduling',
    () {
      final plugin = File(
        'android/src/main/kotlin/dev/aimesoft/erika_flutter/'
        'ErikaFlutterPlugin.kt',
      ).readAsStringSync();

      expect(plugin, contains('private val eventPollRunnable'));
      expect(plugin, contains('private fun scheduleEventPoll()'));
      expect(plugin, contains('presenterThread.post {'));
      expect(plugin, contains('androidEventPollDelayMillis('));

      final frameStart = plugin.indexOf(
        'private val frameCallback = Choreographer.FrameCallback',
      );
      final attachStart = plugin.indexOf(
        'override fun onAttachedToEngine',
        frameStart,
      );
      expect(frameStart, greaterThanOrEqualTo(0));
      expect(attachStart, greaterThan(frameStart));
      expect(
        plugin.substring(frameStart, attachStart),
        isNot(contains('drainEvents')),
      );
    },
  );
}
