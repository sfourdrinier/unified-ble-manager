// Exercises the same codec and control runtime registration used by Android JSI.
#include "../include/NativeProtocolControlRuntime.hpp"

#include <cstdint>
#include <iostream>
#include <string>
#include <vector>

namespace protocol = unified_ble::native_protocol::v2;

int main(int argc, char** argv) {
  const protocol::NativeAttachmentIdentity attachment{
      "rn-boundary-test-attachment", "rn-boundary-test-backend", "1", "rn-boundary-test-adapter", "1"};
  protocol::NativeProtocolControlRuntime runtime(protocol::NativeEpochAllocation::androidScanLanes);
  try {
    static_cast<void>(runtime.handshake(
        attachment, "registry-boundary-test", {protocol::kProtocolVersion, protocol::kProtocolVersion},
        {protocol::kAbiVersion, protocol::kAbiVersion},
        {protocol::kControlSurfaceVersion, protocol::kControlSurfaceVersion},
        {1U, 1U}, {1U, 1U}, {1U, 1U}, {1U, 1U}));
    const bool capacity = argc == 2 && std::string(argv[1]) == "capacity";
    std::optional<protocol::ProtocolRecord> first;
    std::size_t count = 0U;
    std::string line;
    while (std::getline(std::cin, line)) {
      if (line.empty() || line.size() % 2U != 0U) return 2;
      std::vector<std::uint8_t> bytes;
      for (std::size_t i = 0; i < line.size(); i += 2U) {
        bytes.push_back(static_cast<std::uint8_t>(std::stoul(line.substr(i, 2U), nullptr, 16)));
      }
      const auto command = protocol::NativeProtocolV2Codec{}.decode(bytes);
      if (!capacity) {
        try {
          runtime.registerCommand(command, true);
        } catch (const protocol::ProtocolException& error) {
          if (error.failure() != protocol::ProtocolFailure::invalidCorrelation) throw;
          std::cerr << "registry-invalid-correlation: " << error.what() << '\n';
          return 10;
        }
        if (!runtime.rejectCommandDispatch(command)) return 3;
      } else if (count < 1024U) {
        runtime.registerCommand(command, true);
        if (count == 0U) first = command;
      } else if (count == 1024U) {
        bool admitted = true;
        try {
          runtime.registerCommand(command, true, &admitted);
          return 4;
        } catch (const protocol::ProtocolException& error) {
          if (error.failure() != protocol::ProtocolFailure::payloadTooLarge || admitted) return 5;
        }
        if (!first || !runtime.rejectCommandDispatch(*first)) return 6;
      } else if (count == 1025U) {
        bool admitted = false;
        runtime.registerCommand(command, true, &admitted);
        if (!admitted || !runtime.rejectCommandDispatch(command)) return 7;
      } else return 8;
      count += 1U;
    }
    if (capacity && count != 1026U) return 9;
    runtime.close(attachment);
    std::cout << "registered=" << count << '\n';
  } catch (const std::exception& error) {
    std::cerr << error.what() << '\n';
    return 1;
  }
  return 0;
}
