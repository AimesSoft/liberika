cmake_minimum_required(VERSION 3.13)
project(erika_flutter LANGUAGES CXX)
find_package(Threads REQUIRED)
set(PLUGIN_NAME erika_flutter_plugin)
set(ERIKA_LIBRARY_DIR "$ENV{ERIKA_LIBRARY_DIR}" CACHE PATH "Directory containing liberika_capi.so")
if(DEFINED ENV{ERIKA_LIBRARY_DIR})
  set(ERIKA_LIBRARY_DIR "$ENV{ERIKA_LIBRARY_DIR}" CACHE PATH "Directory containing liberika_capi.so" FORCE)
endif()
if(NOT ERIKA_LIBRARY_DIR)
  message(FATAL_ERROR "Build Erika for Linux, then set ERIKA_LIBRARY_DIR to its Cargo target/release directory. See docs/linux.md.")
endif()
unset(ERIKA_CAPI CACHE)
find_library(ERIKA_CAPI erika_capi PATHS "${ERIKA_LIBRARY_DIR}" NO_DEFAULT_PATH REQUIRED)
add_library(${PLUGIN_NAME} SHARED erika_flutter_plugin.cc native_video_surface.cc)
pkg_check_modules(EPOXY REQUIRED IMPORTED_TARGET epoxy)
pkg_check_modules(WAYLAND REQUIRED IMPORTED_TARGET wayland-client)
apply_standard_settings(${PLUGIN_NAME})
set_target_properties(${PLUGIN_NAME} PROPERTIES
  CXX_VISIBILITY_PRESET hidden
  BUILD_WITH_INSTALL_RPATH TRUE
  INSTALL_RPATH "$ORIGIN")
target_compile_features(${PLUGIN_NAME} PRIVATE cxx_std_17)
target_include_directories(${PLUGIN_NAME} INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_include_directories(${PLUGIN_NAME} PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/../native/include")
target_link_libraries(${PLUGIN_NAME} PRIVATE flutter PkgConfig::GTK PkgConfig::EPOXY PkgConfig::WAYLAND Threads::Threads "${ERIKA_CAPI}")
# The bundle is populated after the executable link. Help ld resolve the
# plugin's native dependency at that stage without embedding a build-host path.
target_link_options(${PLUGIN_NAME} INTERFACE "LINKER:-rpath-link,${ERIKA_LIBRARY_DIR}")
set(erika_flutter_bundled_libraries "${ERIKA_CAPI}" PARENT_SCOPE)
