// Test-only bridge: the Android JVM dispatcher drives the real C++ registry.
#include "../include/NativeProtocolControlRuntime.hpp"

#include <iostream>
#include <sstream>
#include <string>
#include <vector>

namespace protocol = unified_ble::native_protocol::v2;

int main() {
  const protocol::NativeAttachmentIdentity attachment{"att-1", "backend-1", "gen-1", "adapter-1", "adaptergen-1"};
  protocol::NativeProtocolControlRuntime runtime(protocol::NativeEpochAllocation::androidScanLanes);
  static_cast<void>(runtime.handshake(attachment, "dispatcher-registry-test",
      {protocol::kProtocolVersion, protocol::kProtocolVersion}, {protocol::kAbiVersion, protocol::kAbiVersion},
      {protocol::kControlSurfaceVersion, protocol::kControlSurfaceVersion}, {1U, 1U}, {1U, 1U}, {1U, 1U}, {1U, 1U}));
  std::string line;
  while (std::getline(std::cin, line)) {
    try {
      if (line == "Q") break;
      if (line.size() < 3U || line[1] != ' ') throw std::runtime_error("invalid test command");
      if (line[0] == 'C' || line[0] == 'T') {
        std::istringstream input(line.substr(2));
        std::uint64_t epoch = 0U;
        std::string nonce;
        input >> epoch >> nonce;
        if (line[0] == 'C') std::cout << protocol::cancellationStateName(runtime.cancel({attachment, epoch, nonce})) << std::endl;
        else std::cout << (runtime.retireActiveScan(epoch, nonce) ? "retired" : "stale") << std::endl;
        continue;
      }
      const auto hex = line.substr(2);
      if (hex.size() % 2U != 0U) throw std::runtime_error("invalid hex");
      std::vector<std::uint8_t> bytes;
      for (std::size_t i = 0; i < hex.size(); i += 2U) {
        bytes.push_back(static_cast<std::uint8_t>(std::stoul(hex.substr(i, 2U), nullptr, 16)));
      }
      const auto record = protocol::NativeProtocolV2Codec{}.decode(bytes);
      if (line[0] == 'R') {
        runtime.registerCommand(record, true);
        std::cout << "registered" << std::endl;
      } else if (line[0] == 'S') {
        std::cout << (runtime.settleResult(record) ? "settled" : "duplicate") << std::endl;
      } else {
        throw std::runtime_error("invalid test operation");
      }
    } catch (const std::exception& error) {
      std::cout << "error:" << error.what() << std::endl;
    }
  }
  runtime.close(attachment);
}
