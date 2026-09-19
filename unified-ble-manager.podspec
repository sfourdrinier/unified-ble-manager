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
    "ios/UnifiedBleRustRadioAdapter.swift",
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

  # PR210-19 build mode (docs/5.0.0-DISTRIBUTION_CONTRACT.md §1, ADR D2.6):
  # explicit configuration, parsed and validated — never inferred. Unset or
  # empty means prebuilt (the CI-built ios/RustCore shipped in the package,
  # no Rust toolchain needed); `prebuilt` and `source` are explicit; any
  # other value stops pod install. Android (android/build.gradle) applies
  # the identical policy.
  ubm_native_build_raw = ENV['UBM_NATIVE_BUILD']
  ubm_native_build_mode =
    if ubm_native_build_raw.nil? || ubm_native_build_raw.empty?
      'prebuilt'
    elsif %w[prebuilt source].include?(ubm_native_build_raw)
      ubm_native_build_raw
    else
      raise Pod::Informative,
            "[unified-ble-manager] UBM_NATIVE_BUILD must be unset, 'prebuilt' or 'source' (got #{ubm_native_build_raw.inspect})."
    end
  # F01: the 5.x lane selects the shared Rust core alongside the Owned
  # radio above. The UniFFI staticlib XCFramework is consumed from
  # ios/RustCore in both modes: in prebuilt mode it is the CI-staged
  # artifact; in source mode the contributor produced it BEFORE pod install
  # with the canonical builder (`UBM_NATIVE_BUILD=source pnpm native:apple:prepare`,
  # i.e. sh ios/build-rust-core.sh). The pod never builds Rust itself:
  # CocoaPods skips prepare_command for :path pods (ADR D2.7), so no hook
  # is used. The generated Swift joins the pod module; the Owned
  # CoreBluetooth radio stays the thin platform adapter calling into it.
  # 4.x keeps the Owned-only selection (no Rust core).
  if package["version"].start_with?("5.")
    s.source_files = base_source_files + ['bindings/uniffi/generated/swift/ubm_echo.swift']
    s.vendored_frameworks = ['ios/RustCore/RustCore.xcframework']
    s.preserve_paths = base_preserve_paths + [
      'bindings/uniffi/generated/swift/ubm_echoFFI.h',
      'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap',
      'ios/verify-rust-core.sh'
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
    # Before any source compiles, in both modes: ios/verify-rust-core.sh
    # (PR210-18) parses the XCFramework Info.plist and checks the exact
    # declared slice set plus the Info.plist and per-slice sha256 recorded in
    # build-identity.json. Source mode additionally rejects a staging whose
    # sealed source digest / binding schema no longer match the sources and
    # prints the command that rebuilds it.
    ubm_verify_script = <<~'UBM_VERIFY_SH'
      set -eu
      RUST_CORE_DIR="${PODS_TARGET_SRCROOT}/ios/RustCore"
      sh "${PODS_TARGET_SRCROOT}/ios/verify-rust-core.sh" --dir "${RUST_CORE_DIR}" || {
        echo "error: [unified-ble-manager] staged ios/RustCore failed verification (see above)." >&2
        exit 1
      }
    UBM_VERIFY_SH
    if ubm_native_build_mode == 'source'
      ubm_verify_script += <<~'UBM_SOURCE_SH'
        # UBM_NATIVE_BUILD=source: the staging must be built from these sources.
        UBM_NODE="${NODE_BINARY:-$(command -v node || true)}"
        [ -n "${UBM_NODE}" ] || {
          echo "error: [unified-ble-manager] UBM_NATIVE_BUILD=source needs Node for the RustCore staleness check (set NODE_BINARY)." >&2
          exit 1
        }
        "${UBM_NODE}" "${PODS_TARGET_SRCROOT}/scripts/release/native-build-identity.js" --root "${PODS_TARGET_SRCROOT}" --check-apple || {
          echo "error: [unified-ble-manager] ios/RustCore is stale for UBM_NATIVE_BUILD=source. Rebuild it, then build again: UBM_NATIVE_BUILD=source pnpm --dir \"${PODS_TARGET_SRCROOT}\" native:apple:prepare" >&2
          exit 1
        }
      UBM_SOURCE_SH
    end
    s.script_phase = {
      :name => 'Verify staged RustCore',
      :execution_position => :before_compile,
      :script => ubm_verify_script
    }
  end
end
