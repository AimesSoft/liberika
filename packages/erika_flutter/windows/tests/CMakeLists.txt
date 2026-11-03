cmake_minimum_required(VERSION 3.14)
project(erika_windows_tests LANGUAGES CXX)

if(NOT EXISTS "${FLUTTER_ENGINE_DIR}/flutter_texture_registrar.h")
  message(FATAL_ERROR "Set FLUTTER_ENGINE_DIR to Flutter's cached Windows engine directory")
endif()
enable_testing()
add_executable(presenter_worker_test presenter_worker_test.cpp)
target_compile_features(presenter_worker_test PRIVATE cxx_std_17)
add_test(NAME presenter_worker COMMAND presenter_worker_test)
set_tests_properties(presenter_worker PROPERTIES TIMEOUT 30)
add_executable(texture_publication_test texture_publication_test.cpp)
target_compile_features(texture_publication_test PRIVATE cxx_std_17)
target_compile_definitions(texture_publication_test PRIVATE NOMINMAX)
target_include_directories(texture_publication_test PRIVATE "${FLUTTER_ENGINE_DIR}")
target_link_libraries(texture_publication_test PRIVATE d3d11 dxgi)
add_test(NAME texture_publication COMMAND texture_publication_test)

# Compile/link the real plugin as well, without needing to create a Flutter
# window or run the Composition graph during these headless native checks.
add_library(plugin_build_check SHARED
  ../erika_flutter_plugin.cpp
  ../erika_flutter_plugin_c_api.cpp
  ../erika_windows_smtc.cpp
  "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/core_implementations.cc"
  "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/plugin_registrar.cc"
  "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/standard_codec.cc")
target_compile_features(plugin_build_check PRIVATE cxx_std_17)
target_compile_options(plugin_build_check PRIVATE /W4 /WX /wd4100 /EHsc /utf-8)
target_compile_definitions(plugin_build_check PRIVATE
  FLUTTER_PLUGIN_IMPL _HAS_EXCEPTIONS=0 _WIN32_WINNT=0x0A00 WINVER=0x0A00
  _SILENCE_EXPERIMENTAL_COROUTINE_DEPRECATION_WARNINGS)
target_include_directories(plugin_build_check PRIVATE
  .. ../../native/include "${FLUTTER_ENGINE_DIR}"
  "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/include")
target_link_libraries(plugin_build_check PRIVATE
  "${FLUTTER_ENGINE_DIR}/flutter_windows.dll.lib"
  runtimeobject windowsapp CoreMessaging d3d11 dxgi shcore shlwapi windowscodecs ole32)

if(ERIKA_COMPAT_DLL OR ERIKA_SOURCE_DLL)
  add_executable(player_lifecycle_test player_lifecycle_test.cpp
    ../erika_windows_smtc.cpp
    "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/core_implementations.cc"
    "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/plugin_registrar.cc"
    "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/standard_codec.cc")
  target_compile_features(player_lifecycle_test PRIVATE cxx_std_17)
  target_compile_options(player_lifecycle_test PRIVATE /EHsc /utf-8)
  target_compile_definitions(player_lifecycle_test PRIVATE
    FLUTTER_PLUGIN_IMPL _HAS_EXCEPTIONS=0 _WIN32_WINNT=0x0A00 WINVER=0x0A00
    _SILENCE_EXPERIMENTAL_COROUTINE_DEPRECATION_WARNINGS)
  target_include_directories(player_lifecycle_test PRIVATE
    .. ../../native/include "${FLUTTER_ENGINE_DIR}"
    "${FLUTTER_ENGINE_DIR}/cpp_client_wrapper/include")
  target_link_libraries(player_lifecycle_test PRIVATE
    "${FLUTTER_ENGINE_DIR}/flutter_windows.dll.lib"
    runtimeobject windowsapp CoreMessaging d3d11 dxgi shcore shlwapi windowscodecs ole32)
  add_custom_command(TARGET player_lifecycle_test POST_BUILD COMMAND ${CMAKE_COMMAND} -E copy_if_different
    "${FLUTTER_ENGINE_DIR}/flutter_windows.dll" "$<TARGET_FILE_DIR:player_lifecycle_test>")
  if(ERIKA_COMPAT_DLL)
    # The released v0.1.7 DLL predates the danmaku-worker join fix. This fixture
    # tests API compatibility, not hot-unloading that unpatched binary.
    add_test(NAME released_runtime_compatibility COMMAND player_lifecycle_test
      "${ERIKA_COMPAT_DLL}" --keep-runtime-loaded)
  endif()
  if(ERIKA_SOURCE_DLL)
    add_test(NAME source_texture_lifecycle COMMAND player_lifecycle_test "${ERIKA_SOURCE_DLL}")
    set_tests_properties(source_texture_lifecycle PROPERTIES TIMEOUT 45)
  endif()
endif()
