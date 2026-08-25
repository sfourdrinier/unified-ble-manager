# android/src/main/jni/CMakeLists.txt

cmake_minimum_required(VERSION 3.20)
project(unified_ble_native_protocol LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

set(UNIFIED_BLE_PROJECT_BUILD_DIRECTORY "${PROJECT_BUILD_DIR}")
set(UNIFIED_BLE_PROJECT_ROOT_DIRECTORY "${PROJECT_ROOT_DIR}")
set(UNIFIED_BLE_REACT_ANDROID_DIRECTORY "${REACT_ANDROID_DIR}")

find_package(ReactAndroid REQUIRED CONFIG)
find_package(fbjni REQUIRED CONFIG)

add_library(
  ${CMAKE_PROJECT_NAME}
  SHARED
  ../../../../native/protocol/src/NativeProtocolV2Codec.cpp
  ../../../../native/protocol/src/NativeProtocolControlRuntime.cpp
  ../../../../native/protocol/src/NativeProtocolV2Registry.cpp
  ../../../../native/protocol/src/OwnedBinaryPayloadStore.cpp
  ../../../../native/protocol/src/OwnedJsiBinaryTransport.cpp
  UnifiedBleProtocolControlJni.cpp
  UnifiedBleProtocolJsiBinding.cpp
)

target_include_directories(
  ${CMAKE_PROJECT_NAME}
  PRIVATE
  ../../../../native/protocol/include
  ../../../../native/protocol/generated
)

target_link_libraries(
  ${CMAKE_PROJECT_NAME}
  ReactAndroid::jsi
  ReactAndroid::reactnative
  fbjni::fbjni
  log
)

target_compile_options(
  ${CMAKE_PROJECT_NAME}
  PRIVATE
  -Wall
  -Wextra
  -Werror
)

# 16 KB page size support.
# Android 15+ devices may use 16 KB memory pages, and Google Play requires
# 16 KB-compatible native libraries. NDK r27.1+ links this way by default,
# but stating it explicitly keeps the artifact correct on any NDK.
target_link_options(
  ${CMAKE_PROJECT_NAME}
  PRIVATE
  "-Wl,-z,max-page-size=16384"
  "-Wl,-z,common-page-size=16384"
)
