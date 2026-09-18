# unified-ble-manager.podspec

require "json"

package = JSON.parse(File.read(File.join(__dir__, "package.json")))
folly_compiler_flags = '-DFOLLY_NO_CONFIG -DFOLLY_MOBILE=1 -DFOLLY_USE_LIBCPP=1 -Wno-comma -Wno-shorten-64-to-32'

Pod::Spec.new do |s|
  s.name         = "unified-ble-manager"
  s.version      = package["version"]
  s.summary      = package["description"]
  s.homepage     = package["homepage"]
  s.license      = package["license"]
  s.authors      = package["author"]

  s.platforms    = { :ios => "16.4", :tvos => "16.4" }
  s.source       = { :git => "https://github.com/sfourdrinier/unified-ble-manager.git", :tag => "v#{s.version}" }

  # The 4.0 product is the Unified BLE Protocol control module and its owned
  # CoreBluetooth radio. Keep this list explicit: the retired 3.x bridge must never
  # be pulled into an Apple target through a future glob expansion.
  s.module_name  = "BlePlx"
  # Hoisted locals: the 5.x branch below extends both lists, and `+=` on
  # the attributes fails (no getter in the installed CocoaPods) — plain
  # `=` from locals works everywhere.
  base_source_files = [
    "ios/UnifiedBleProtocolControl.mm",
    "ios/UnifiedBleExpoRuntime.mm",
    "ios/UnifiedBleRustCore.mm",
    "ios/UnifiedBleRustCoreSessions.swift",
    "ios/UnifiedBleRustCoreAdapterState.swift",
    "ios/Generated/**/*.swift",
    "ios/NativeProtocol/**/*.{h,m,mm}",
    "ios/Owned/OwnedCoreBluetoothCentralDelegate.swift",
    "ios/Owned/OwnedCoreBluetoothProtocolRadioDescriptors.swift",
    "ios/Owned/OwnedCoreBluetoothProtocolRadio.swift",
    "ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift",
    "ios/Owned/OwnedCoreBluetoothProtocolRadioOwner.swift",
    "ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift",
    "native/protocol/src/**/*.cpp"
  ]
  s.source_files = base_source_files
  base_preserve_paths = [
    "native/protocol/include/**/*.hpp",
    "native/protocol/generated/**/*.hpp"
  ]
  s.preserve_paths = base_preserve_paths
  s.resource_bundles = { 'BlePlx' => ['ios/PrivacyInfo.xcprivacy'] }
  s.frameworks = "CoreBluetooth", "Security"
  # Do not add -fmodules/-fcxx-modules: React Native's source build owns the
  # C++ module configuration and duplicate definitions otherwise become possible.
  s.compiler_flags = "-DOWNED_COREBLUETOOTH_RADIO=1"
  s.pod_target_xcconfig = {
    "CLANG_CXX_LANGUAGE_STANDARD" => "c++20"
  }

  # Use install_modules_dependencies helper to install the dependencies if React Native version >=0.71.0.
  if respond_to?(:install_modules_dependencies, true)
    install_modules_dependencies(s)
  else
    s.dependency "React-Core"
    s.compiler_flags = folly_compiler_flags + " -DRCT_NEW_ARCH_ENABLED=1"
    s.pod_target_xcconfig = {
      "HEADER_SEARCH_PATHS" => "\"$(PODS_ROOT)/boost\"",
      "OTHER_CPLUSPLUSFLAGS" => "-DFOLLY_NO_CONFIG -DFOLLY_MOBILE=1 -DFOLLY_USE_LIBCPP=1",
      "CLANG_CXX_LANGUAGE_STANDARD" => "c++20"
    }
    s.dependency "React-Codegen"
    s.dependency "RCT-Folly"
    s.dependency "RCTRequired"
    s.dependency "RCTTypeSafety"
    s.dependency "ReactCommon/turbomodule/core"
  end

  # D2 distribution modes (docs/5.0.0-DISTRIBUTION_CONTRACT.md): prebuilt
  # is the default — the shipped ios/RustCore artifacts are consumed as
  # built by CI, with no toolchain needed on the consumer. Only the
  # explicit UBM_NATIVE_BUILD=source selects a from-source build of the
  # same artifacts with the canonical builder. Never inferred: any other
  # value (including empty) means prebuilt.
  ubm_native_source_build = ENV['UBM_NATIVE_BUILD'] == 'source'
  # F01: the 5.x lane selects the shared Rust core alongside the Owned
  # radio above. The UniFFI staticlib XCFramework ships staged in
  # ios/RustCore (prebuilt mode) or is built from the shipped
  # bindings/uniffi sources by ios/build-rust-core.sh (source mode); the
  # generated Swift joins the pod module; the Owned CoreBluetooth radio
  # stays the thin platform adapter calling into it.
  # 4.x keeps the Owned-only selection (no Rust core).
  if package["version"].start_with?("5.")
    # Source mode builds via prepare_command (observed to run during pod
    # install for the example consumers; the CocoaPods guides claim :path
    # pods skip it, so the Verify phase below stays the
    # mechanism-independent backstop with actionable errors either way).
    if ubm_native_source_build
      s.prepare_command = 'sh ios/build-rust-core.sh'
    end
    s.source_files = base_source_files + ['bindings/uniffi/generated/swift/ubm_echo.swift']
    s.vendored_frameworks = ['ios/RustCore/RustCore.xcframework']
    s.preserve_paths = base_preserve_paths + [
      'bindings/uniffi/generated/swift/ubm_echoFFI.h',
      'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap'
    ]
    # ubm_echo.swift compiles in the POD target, so the ubm_echoFFI
    # modulemap (which defines RustBuffer and friends) must be visible
    # here — s.xcconfig reaches only the consumer target and leaves
    # `canImport(ubm_echoFFI)` false at pod compile time. SWIFT_INCLUDE_PATHS
    # alone is not enough: Clang only auto-loads `module.modulemap` from
    # search paths, never a custom-named `ubm_echoFFI.modulemap`, so the
    # modulemap must also be passed explicitly via -fmodule-map-file.
    # Pod::Specification has no getters: read-modify-write via
    # to_hash, the same pattern React Native's
    # install_modules_dependencies uses, so neither the helper branch
    # nor the legacy branch assignments are clobbered.
    ubm_pod_xcconfig = s.to_hash['pod_target_xcconfig'] || {}
    ubm_module_map_flag = '-Xcc -fmodule-map-file=$(PODS_TARGET_SRCROOT)/bindings/uniffi/generated/swift/ubm_echoFFI.modulemap'
    ubm_existing_swift_flags = ubm_pod_xcconfig['OTHER_SWIFT_FLAGS']
    ubm_swift_flags = (ubm_existing_swift_flags.nil? || ubm_existing_swift_flags.empty?) \
      ? ubm_module_map_flag \
      : "#{ubm_existing_swift_flags} #{ubm_module_map_flag}"
    s.pod_target_xcconfig = ubm_pod_xcconfig.merge(
      'SWIFT_INCLUDE_PATHS' => '$(PODS_TARGET_SRCROOT)/bindings/uniffi/generated/swift',
      'OTHER_SWIFT_FLAGS' => ubm_swift_flags
    )
    # Mechanism-independent backstop: whatever produced ios/RustCore
    # (prepare_command above, an explicit contributor build, or CI
    # staging), the compile fails here — before any source compiles —
    # with an actionable error when the staging is missing or partial.
    s.script_phase = {
      :name => 'Verify staged RustCore',
      :execution_position => :before_compile,
      :script => <<~'UBM_VERIFY_SH'
        set -eu
        RUST_CORE_DIR="${PODS_TARGET_SRCROOT}/ios/RustCore"
        FRAMEWORK="${RUST_CORE_DIR}/RustCore.xcframework"
        IDENTITY="${RUST_CORE_DIR}/build-identity.txt"
        fail() { echo "error: [unified-ble-manager] $1" >&2; exit 1; }
        [ -d "${FRAMEWORK}" ] || fail "missing ${FRAMEWORK}. Prebuilt mode (default) needs the CI-staged XCFramework in the package: reinstall or upgrade unified-ble-manager. For a from-source build instead: UBM_NATIVE_BUILD=source sh ios/build-rust-core.sh from the package root, then pod install."
        [ -f "${IDENTITY}" ] || fail "missing ${IDENTITY}: the RustCore staging is incomplete; restage it (see above)."
        SLICES=$(grep -c 'LibraryIdentifier' "${FRAMEWORK}/Info.plist" 2>/dev/null || true)
        [ "${SLICES}" = '4' ] || fail "expected 4 platform slices in ${FRAMEWORK}/Info.plist, found '${SLICES}': restage RustCore (see above)."
      UBM_VERIFY_SH
    }
  end
end
