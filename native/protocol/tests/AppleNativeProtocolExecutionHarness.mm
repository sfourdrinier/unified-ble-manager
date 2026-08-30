// native/protocol/tests/AppleNativeProtocolExecutionHarness.mm

// This intentionally includes the production implementation: the harness
// drives its actual State, scheduleRecord, CallInvoker drain, JSI sink, and
// NativeProtocolControlRuntime settlement path in one translation unit.
#import <Foundation/Foundation.h>

@interface OwnedCoreBluetoothProtocolRadio : NSObject
- (void)releaseProtocolClientWithCompletion:(void (^)(NSError*))completion;
- (void)startScanWithServiceUUIDs:(NSArray<NSString*>*)serviceUuids allowDuplicates:(BOOL)allowDuplicates operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)stopScanWithOperationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)connectWithPeerIdentifier:(NSString*)peer operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)disconnectWithPeerIdentifier:(NSString*)peer operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)discoverWithPeerIdentifier:(NSString*)peer operationIdentifier:(NSString*)operation completion:(void (^)(NSDictionary*, NSError*))completion;
- (void)readRssiWithPeerIdentifier:(NSString*)peer operationIdentifier:(NSString*)operation completion:(void (^)(NSNumber*, NSError*))completion;
- (void)readDescriptorWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence descriptorUUID:(NSString*)descriptor descriptorOccurrence:(NSInteger)descriptorOccurrence operationIdentifier:(NSString*)operation completion:(void (^)(NSData*, NSError*))completion;
- (void)writeDescriptorWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence descriptorUUID:(NSString*)descriptor descriptorOccurrence:(NSInteger)descriptorOccurrence value:(NSData*)value operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)readWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence operationIdentifier:(NSString*)operation completion:(void (^)(NSData*, NSError*))completion;
- (void)writeWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence value:(NSData*)value withResponse:(BOOL)withResponse operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)subscribeWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence subscriptionIdentifier:(NSString*)subscription operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)unsubscribeWithPeerIdentifier:(NSString*)peer serviceUUID:(NSString*)service serviceOccurrence:(NSInteger)serviceOccurrence characteristicUUID:(NSString*)characteristic characteristicOccurrence:(NSInteger)characteristicOccurrence subscriptionIdentifier:(NSString*)subscription operationIdentifier:(NSString*)operation completion:(void (^)(NSError*))completion;
- (void)cancelOperation:(NSString*)operation completion:(void (^)(NSDictionary*))completion;
- (NSArray<NSString*>*)restorationPeerIdentifiers;
@end

@interface ImmediateDisconnectRadio : NSObject
@end

@implementation ImmediateDisconnectRadio

- (void)releaseProtocolClientWithCompletion:(void (^)(NSError*))completion {
  completion(nil);
}

- (void)connectWithPeerIdentifier:(NSString*)peer
                 operationIdentifier:(NSString*)operation
                          completion:(void (^)(NSError*))completion {
  completion(nil);
}

@end

#include "../../../ios/NativeProtocol/UnifiedBleProtocolAppleExecution.mm"

#include <ReactCommon/CallInvoker.h>
#include <jsc/JSCRuntime.h>
#include <jsi/instrumentation.h>

#include <atomic>
#include <barrier>
#include <cstdint>
#include <iostream>
#include <memory>
#include <mutex>
#include <thread>
#include <utility>
#include <vector>

namespace {

using facebook::jsi::Function;
using facebook::jsi::JSError;
using facebook::jsi::PropNameID;
using facebook::jsi::Runtime;
using facebook::jsi::Value;
using facebook::react::CallFunc;
using facebook::react::CallInvoker;
using unified_ble::apple_protocol::AppleNativeProtocolExecution;
namespace protocol = unified_ble::native_protocol::v2;

protocol::VersionRange nativeProtocolRange() {
  return {.minimum = protocol::kProtocolVersion, .maximum = protocol::kProtocolVersion};
}

protocol::VersionRange abiVersionRange() {
  return {.minimum = protocol::kAbiVersion, .maximum = protocol::kAbiVersion};
}

protocol::VersionRange controlSurfaceVersionRange() {
  return {.minimum = protocol::kControlSurfaceVersion, .maximum = protocol::kControlSurfaceVersion};
}

class ControllableInvoker final : public CallInvoker {
 public:
  void invokeAsync(CallFunc&& function) noexcept override {
    std::lock_guard<std::mutex> lock(mutex_);
    scheduled_.push_back(std::move(function));
  }

  void invokeSync(CallFunc&& function) override {
    function(runtime_);
  }

  std::size_t pending() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return scheduled_.size();
  }

  void flushOne() {
    CallFunc function;
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (scheduled_.empty()) return;
      function = std::move(scheduled_.front());
      scheduled_.erase(scheduled_.begin());
    }
    function(runtime_);
  }

  explicit ControllableInvoker(Runtime& runtime) : runtime_(runtime) {}

 private:
  Runtime& runtime_;
  mutable std::mutex mutex_;
  std::vector<CallFunc> scheduled_;
};

protocol::ProtocolField harnessField(std::uint16_t identifier, protocol::ProtocolFieldValue value) {
  return {.id = identifier, .value = std::move(value)};
}

protocol::ProtocolRecordReference attachment() {
  return std::make_shared<protocol::ProtocolRecord>(protocol::ProtocolRecord{
      .kind = protocol::RecordKind::attachment,
      .fields = {
          harnessField(1U, std::string("apple-execution-attachment")),
          harnessField(2U, std::string("apple-execution-backend")),
          harnessField(3U, std::string("apple-execution-generation")),
          harnessField(4U, std::string("apple-execution-adapter")),
          harnessField(5U, std::string("apple-execution-adapter-generation")),
      },
  });
}

protocol::ProtocolRecord command(std::uint64_t epoch) {
  return {
      .kind = protocol::RecordKind::command,
      .fields = {
          harnessField(1U, std::uint64_t{protocol::kProtocolVersion}),
          harnessField(2U, std::make_shared<protocol::ProtocolRecord>(protocol::ProtocolRecord{
              .kind = protocol::RecordKind::operationCorrelation,
              .fields = {
                  harnessField(1U, attachment()),
                  harnessField(2U, epoch),
                  harnessField(3U, std::string("apple-execution-operation-") + std::to_string(epoch)),
              },
          })),
          harnessField(3U, std::string("destroy")),
      },
  };
}

protocol::ProtocolRecord enumerateBondedPeersCommand(std::uint64_t epoch) {
  return {
      .kind = protocol::RecordKind::command,
      .fields = {
          harnessField(1U, std::uint64_t{protocol::kProtocolVersion}),
          harnessField(2U, std::make_shared<protocol::ProtocolRecord>(protocol::ProtocolRecord{
              .kind = protocol::RecordKind::operationCorrelation,
              .fields = {
                  harnessField(1U, attachment()),
                  harnessField(2U, epoch),
                  harnessField(3U, std::string("apple-enumerate-bonded-operation-") + std::to_string(epoch)),
              },
          })),
          harnessField(3U, std::string("enumerateBondedPeers")),
      },
  };
}

const protocol::ProtocolRecord* harnessRecordField(
    const protocol::ProtocolRecord& record,
    std::uint16_t identifier) {
  for (const auto& candidate : record.fields) {
    if (candidate.id != identifier) continue;
    const auto* value = std::get_if<protocol::ProtocolRecordReference>(&candidate.value);
    return value == nullptr || !*value ? nullptr : value->get();
  }
  return nullptr;
}

const std::string* harnessStringField(
    const protocol::ProtocolRecord& record,
    std::uint16_t identifier) {
  for (const auto& candidate : record.fields) {
    if (candidate.id != identifier) continue;
    return std::get_if<std::string>(&candidate.value);
  }
  return nullptr;
}

protocol::ProtocolRecord connectCommand(
    std::uint64_t epoch,
    const std::string& peer,
    const std::string& connectionGeneration = "connection-generation-1",
    const std::string& operationPrefix = "apple-immediate-disconnect-operation-") {
  return {
      .kind = protocol::RecordKind::command,
      .fields = {
          harnessField(1U, std::uint64_t{protocol::kProtocolVersion}),
          harnessField(2U, std::make_shared<protocol::ProtocolRecord>(protocol::ProtocolRecord{
              .kind = protocol::RecordKind::operationCorrelation,
              .fields = {
                  harnessField(1U, attachment()),
                  harnessField(2U, epoch),
                  harnessField(3U, operationPrefix + std::to_string(epoch)),
              },
          })),
          harnessField(3U, std::string("connect")),
          harnessField(10U, std::make_shared<protocol::ProtocolRecord>(protocol::ProtocolRecord{
              .kind = protocol::RecordKind::connectionPath,
              .fields = {
                  harnessField(1U, attachment()),
                  harnessField(2U, peer),
                  harnessField(3U, std::string("connection-1")),
                  harnessField(4U, std::string("lease-1")),
                  harnessField(5U, connectionGeneration),
              },
          })),
      },
  };
}

std::shared_ptr<protocol::NativeProtocolControlRuntime> openedRuntime() {
  const auto runtime = std::make_shared<protocol::NativeProtocolControlRuntime>();
  static_cast<void>(runtime->handshake(
      {
          .attachmentId = "apple-execution-attachment",
          .backendInstanceId = "apple-execution-backend",
          .backendGeneration = "apple-execution-generation",
          .adapterId = "apple-execution-adapter",
          .adapterGeneration = "apple-execution-adapter-generation",
      },
      "apple-execution-owner",
      nativeProtocolRange(), abiVersionRange(), controlSurfaceVersionRange(),
      {1U, 1U},
      {1U, 1U},
      {1U, 1U},
      {1U, 1U}));
  return runtime;
}

void initializeAttachment(
    const std::shared_ptr<AppleNativeProtocolExecution::State>& state,
    const std::shared_ptr<CallInvoker>& invoker,
    const std::shared_ptr<Function>& sink) {
  std::scoped_lock lock(state->mutex);
  state->attachmentActive = true;
  state->attachmentGeneration = 1U;
  state->callInvoker = invoker;
  state->eventSink = sink;
}

void releaseHarnessJsiState(
    const std::shared_ptr<AppleNativeProtocolExecution::State>& state) {
  std::scoped_lock lock(state->mutex);
  state->eventSink.reset();
  state->fatalSink.reset();
  state->sinksAwaitingJavaScriptRelease.clear();
  state->callInvoker.reset();
}

bool require(bool condition, const char* message) {
  if (condition) return true;
  std::cerr << message << '\n';
  return false;
}

} // namespace

namespace unified_ble::apple_protocol {

int runAppleNativeProtocolExecutionHarness() {
  try {
    if (!require(parseAppleGattOccurrence("0", "test") == 0, "canonical zero occurrence was rejected")) return 1;
    for (const auto& invalid : {std::string(" 1"), std::string("+1"), std::string("1 "), std::string("-1"), std::string("1x")}) {
      try {
        static_cast<void>(parseAppleGattOccurrence(invalid, "test"));
        return require(false, "non-canonical Apple GATT occurrence was accepted") ? 0 : 1;
      } catch (const protocol::ProtocolException&) {
      }
    }

    auto runtime = facebook::jsc::makeJSCRuntime();
    int harnessResult = 1;
    // A lambda gives every early failure one teardown path: all JSI wrappers,
    // installed host objects, invoker-owned callbacks, and autoreleased
    // Objective-C completion objects die before the JSCRuntime is explicitly
    // destroyed. Returning directly from this body previously left that order
    // implicit and JSC found a dangling API object during its destructor.
    @autoreleasepool {
    harnessResult = [&]() -> int {
    std::atomic<std::size_t> delivered{0U};
    const auto sink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "appleExecutionSink"),
        1U,
        [&delivered](Runtime&, const Value&, const Value*, std::size_t) {
          delivered.fetch_add(1U, std::memory_order_relaxed);
          return Value::undefined();
        }));
    const auto invoker = std::make_shared<ControllableInvoker>(*runtime);
    invoker->invokeAsync([](Runtime&) {});
    if (!require(invoker->pending() == 1U, "controllable invoker did not retain a direct callback")) return 1;
    invoker->flushOne();
    const auto control = openedRuntime();
    const auto state = std::make_shared<AppleNativeProtocolExecution::State>(control, nullptr);
    initializeAttachment(state, invoker, sink);

    const auto first = command(1U);
    const auto second = command(2U);
    control->registerCommand(first, true);
    control->registerCommand(second, true);
    std::barrier startTerminals(3);
    std::exception_ptr firstTerminalFailure;
    std::exception_ptr secondTerminalFailure;
    std::thread firstTerminal([&] {
      startTerminals.arrive_and_wait();
      try {
        static_cast<void>(success(state, first));
      } catch (...) {
        firstTerminalFailure = std::current_exception();
      }
    });
    std::thread secondTerminal([&] {
      startTerminals.arrive_and_wait();
      try {
        static_cast<void>(success(state, second));
      } catch (...) {
        secondTerminalFailure = std::current_exception();
      }
    });
    startTerminals.arrive_and_wait();
    firstTerminal.join();
    secondTerminal.join();
    if (firstTerminalFailure != nullptr) std::rethrow_exception(firstTerminalFailure);
    if (secondTerminalFailure != nullptr) std::rethrow_exception(secondTerminalFailure);
    if (!require(invoker->pending() == 1U, "concurrent terminals scheduled more than one Apple drain")) return 1;
    if (!require(delivered.load(std::memory_order_relaxed) == 0U, "terminal settled or delivered before the controllable invoker ran")) return 1;
    invoker->flushOne();
    if (!require(delivered.load(std::memory_order_relaxed) == 2U, "concurrent terminals were not each delivered once")) return 1;
    if (!require(!control->commandFor(1U, "apple-execution-operation-1").has_value(), "first delivered terminal did not settle once")) return 1;
    if (!require(!control->commandFor(2U, "apple-execution-operation-2").has_value(), "second delivered terminal did not settle once")) return 1;
    {
      std::scoped_lock lock(state->mutex);
      if (!require(!state->attachmentFatal && !state->ingressClosed, "successful concurrent terminals fatally closed the attachment")) return 1;
    }

    const auto missingInvokerControl = openedRuntime();
    const auto missingInvokerState = std::make_shared<AppleNativeProtocolExecution::State>(missingInvokerControl, nullptr);
    initializeAttachment(missingInvokerState, nullptr, sink);
    const auto unavailableCommand = command(1U);
    missingInvokerControl->registerCommand(unavailableCommand, true);
    static_cast<void>(success(missingInvokerState, unavailableCommand));
    if (!require(!missingInvokerControl->open(), "actual scheduling-unavailable seam did not fatally close the attachment")) return 1;

    const auto enumerateControl = openedRuntime();
    const auto enumerateInvoker = std::make_shared<ControllableInvoker>(*runtime);
    std::atomic<std::size_t> enumerateDelivered{0U};
    std::atomic<std::size_t> enumerateFailed{0U};
    std::atomic<std::size_t> enumerateCaused{0U};
    std::atomic<std::size_t> enumerateUnsupported{0U};
    std::atomic<std::size_t> enumerateCorrelated{0U};
    const auto enumerateSink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "appleEnumerateBondedPeersSink"),
        1U,
        [&enumerateDelivered, &enumerateFailed, &enumerateCaused, &enumerateUnsupported, &enumerateCorrelated](
            Runtime& inner, const Value&, const Value* arguments, std::size_t count) {
          if (count != 1U || !arguments[0].isObject() || !arguments[0].asObject(inner).isUint8Array(inner)) {
            return Value::undefined();
          }
          auto array = arguments[0].asObject(inner).asUint8Array(inner);
          const auto buffer = array.buffer(inner);
          const auto offset = array.byteOffset(inner);
          const auto length = array.byteLength(inner);
          if (buffer.detached(inner) || offset > buffer.size(inner) || length > buffer.size(inner) - offset) {
            return Value::undefined();
          }
          const auto* source = buffer.data(inner);
          if (source == nullptr && length != 0U) return Value::undefined();
          const auto bytes = length == 0U
              ? std::vector<std::uint8_t>{}
              : std::vector<std::uint8_t>(source + offset, source + offset + length);
          const auto record = protocol::NativeProtocolV2Codec{}.decode(bytes);
          if (record.kind != protocol::RecordKind::result) return Value::undefined();
          enumerateDelivered.fetch_add(1U, std::memory_order_relaxed);
          const auto* terminal = harnessRecordField(record, 3U);
          const auto* outcome = terminal == nullptr ? nullptr : harnessStringField(*terminal, 2U);
          if (outcome != nullptr && *outcome == "failed") enumerateFailed.fetch_add(1U, std::memory_order_relaxed);
          const auto* cause = terminal == nullptr ? nullptr : harnessStringField(*terminal, 3U);
          if (cause != nullptr && *cause == "unsupportedCommand") enumerateCaused.fetch_add(1U, std::memory_order_relaxed);
          const auto* error = harnessRecordField(record, 10U);
          const auto* code = error == nullptr ? nullptr : harnessStringField(*error, 1U);
          if (code != nullptr && *code == "unsupportedCommand") enumerateUnsupported.fetch_add(1U, std::memory_order_relaxed);
          const auto* correlation = terminal == nullptr ? nullptr : harnessRecordField(*terminal, 1U);
          const auto* nonce = correlation == nullptr ? nullptr : harnessStringField(*correlation, 3U);
          if (nonce != nullptr && *nonce == "apple-enumerate-bonded-operation-3") {
            enumerateCorrelated.fetch_add(1U, std::memory_order_relaxed);
          }
          return Value::undefined();
        }));
    AppleNativeProtocolExecution enumerateExecution(enumerateControl, nullptr);
    enumerateExecution.install(*runtime, enumerateInvoker);
    enumerateExecution.beginAttachment();
    const auto enumerateInstalled = runtime->global().getProperty(*runtime, "__unifiedBleNativeProtocolV2");
    const auto enumerateObject = enumerateInstalled.asObject(*runtime);
    const auto enumerateSetSink = enumerateObject.getProperty(*runtime, "setEventSink").asObject(*runtime).asFunction(*runtime);
    enumerateSetSink.call(*runtime, *enumerateSink);
    const auto enumerateCommand = enumerateBondedPeersCommand(3U);
    const auto enumerateBytes = protocol::NativeProtocolV2Codec{}.encode(enumerateCommand);
    auto enumerateArray = jsi::Uint8Array(*runtime, enumerateBytes.size());
    auto enumerateBuffer = enumerateArray.buffer(*runtime);
    std::memcpy(enumerateBuffer.data(*runtime), enumerateBytes.data(), enumerateBytes.size());
    const auto enumerateSubmit = enumerateObject.getProperty(*runtime, "submit").asObject(*runtime).asFunction(*runtime);
    enumerateSubmit.call(*runtime, enumerateArray);
    if (!require(
            enumerateControl->commandFor(3U, "apple-enumerate-bonded-operation-3").has_value(),
            "Apple enumerateBondedPeers command was not retained before terminal delivery")) return 1;
    if (!require(enumerateInvoker->pending() == 1U, "Apple enumerateBondedPeers scheduled an unexpected number of drains")) return 1;
    enumerateInvoker->flushOne();
    if (!require(enumerateDelivered.load(std::memory_order_relaxed) == 1U, "Apple enumerateBondedPeers did not deliver exactly one result")) return 1;
    if (!require(enumerateFailed.load(std::memory_order_relaxed) == 1U, "Apple enumerateBondedPeers did not deliver a failed terminal")) return 1;
    if (!require(enumerateCaused.load(std::memory_order_relaxed) == 1U, "Apple enumerateBondedPeers failure did not carry unsupportedCommand cause")) return 1;
    if (!require(enumerateUnsupported.load(std::memory_order_relaxed) == 1U, "Apple enumerateBondedPeers did not report unsupportedCommand")) return 1;
    if (!require(enumerateCorrelated.load(std::memory_order_relaxed) == 1U, "Apple enumerateBondedPeers failure was not correlated")) return 1;
    if (!require(
            !enumerateControl->commandFor(3U, "apple-enumerate-bonded-operation-3").has_value(),
            "Apple enumerateBondedPeers terminal did not settle the operation")) return 1;
    enumerateExecution.close();
    while (enumerateInvoker->pending() != 0U) enumerateInvoker->flushOne();

    const auto immediateDisconnectControl = openedRuntime();
    const auto immediateDisconnectInvoker = std::make_shared<ControllableInvoker>(*runtime);
    std::atomic<std::size_t> connectionLostEvents{0U};
    const auto immediateDisconnectSink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "appleImmediateDisconnectSink"),
        1U,
        [&connectionLostEvents](Runtime& inner, const Value&, const Value* arguments, std::size_t count) {
          if (count != 1U || !arguments[0].isObject() || !arguments[0].asObject(inner).isUint8Array(inner)) return Value::undefined();
          auto array = arguments[0].asObject(inner).asUint8Array(inner);
          auto buffer = array.buffer(inner);
          const auto offset = array.byteOffset(inner);
          const auto length = array.byteLength(inner);
          if (buffer.detached(inner) || offset > buffer.size(inner) || length > buffer.size(inner) - offset) return Value::undefined();
          const auto* source = buffer.data(inner);
          if (source == nullptr && length != 0U) return Value::undefined();
          const auto bytes = length == 0U
              ? std::vector<std::uint8_t>{}
              : std::vector<std::uint8_t>(source + offset, source + offset + length);
          const auto record = protocol::NativeProtocolV2Codec{}.decode(bytes);
          if (record.kind != protocol::RecordKind::event) return Value::undefined();
          for (const auto& field : record.fields) {
            if (field.id == 3U && std::holds_alternative<std::string>(field.value) &&
                std::get<std::string>(field.value) == "connectionLost") {
              connectionLostEvents.fetch_add(1U, std::memory_order_relaxed);
              break;
            }
          }
          return Value::undefined();
        }));
    auto* immediateDisconnectRadio = [[ImmediateDisconnectRadio alloc] init];
    AppleNativeProtocolExecution immediateDisconnectExecution(
        immediateDisconnectControl,
        (__bridge void*)immediateDisconnectRadio);
    immediateDisconnectExecution.install(*runtime, immediateDisconnectInvoker);
    immediateDisconnectExecution.beginAttachment();
    const auto installedRuntime = runtime->global().getProperty(*runtime, "__unifiedBleNativeProtocolV2");
    const auto installedObject = installedRuntime.asObject(*runtime);
    const auto setEventSink = installedObject.getProperty(*runtime, "setEventSink").asObject(*runtime).asFunction(*runtime);
    setEventSink.call(*runtime, *immediateDisconnectSink);
    const auto immediateDisconnectCommand = connectCommand(1U, "peer-immediate-disconnect");
    const auto commandBytes = protocol::NativeProtocolV2Codec{}.encode(immediateDisconnectCommand);
    auto commandArray = jsi::Uint8Array(*runtime, commandBytes.size());
    auto commandBuffer = commandArray.buffer(*runtime);
    std::memcpy(commandBuffer.data(*runtime), commandBytes.data(), commandBytes.size());
    const auto submit = installedObject.getProperty(*runtime, "submit").asObject(*runtime).asFunction(*runtime);
    submit.call(*runtime, commandArray);
    NSString* immediateDisconnectPeer = @"peer-immediate-disconnect";
    NSError* immediateDisconnectError = [NSError errorWithDomain:@"ImmediateDisconnectTest" code:17 userInfo:nil];
    immediateDisconnectExecution.receiveDisconnect(
        (__bridge void*)immediateDisconnectPeer,
        (__bridge void*)immediateDisconnectError);
    immediateDisconnectExecution.receiveDisconnect(
        (__bridge void*)immediateDisconnectPeer,
        (__bridge void*)immediateDisconnectError);
    if (!require(immediateDisconnectInvoker->pending() == 1U, "immediate disconnect race scheduled an unexpected number of drains before JavaScript delivery")) return 1;
    immediateDisconnectInvoker->flushOne();
    if (!require(immediateDisconnectInvoker->pending() == 1U, "immediate disconnect was not admitted behind connect settlement")) return 1;
    immediateDisconnectInvoker->flushOne();
    if (!require(connectionLostEvents.load(std::memory_order_relaxed) == 1U, "immediate disconnect was lost before JavaScript installed connection ownership")) return 1;
    immediateDisconnectExecution.close();
    while (immediateDisconnectInvoker->pending() != 0U) immediateDisconnectInvoker->flushOne();

    const auto failedConnectControl = openedRuntime();
    const auto failedConnectInvoker = std::make_shared<ControllableInvoker>(*runtime);
    std::atomic<std::size_t> staleConnectionLostEvents{0U};
    const auto failedConnectSink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "appleFailedConnectSink"),
        1U,
        [&staleConnectionLostEvents](Runtime& inner, const Value&, const Value* arguments, std::size_t count) {
          if (count != 1U || !arguments[0].isObject() || !arguments[0].asObject(inner).isUint8Array(inner)) return Value::undefined();
          auto bytes = arguments[0].asObject(inner).asUint8Array(inner);
          auto buffer = bytes.buffer(inner);
          if (buffer.detached(inner) || bytes.byteOffset(inner) > buffer.size(inner) || bytes.byteLength(inner) > buffer.size(inner) - bytes.byteOffset(inner)) return Value::undefined();
          const auto* source = buffer.data(inner);
          if (source == nullptr && bytes.byteLength(inner) != 0U) return Value::undefined();
          const auto record = protocol::NativeProtocolV2Codec{}.decode(
              bytes.byteLength(inner) == 0U
                  ? std::vector<std::uint8_t>{}
                  : std::vector<std::uint8_t>(source + bytes.byteOffset(inner), source + bytes.byteOffset(inner) + bytes.byteLength(inner)));
          if (record.kind != protocol::RecordKind::event) return Value::undefined();
          for (const auto& field : record.fields) {
            if (field.id == 3U && std::holds_alternative<std::string>(field.value) &&
                std::get<std::string>(field.value) == "connectionLost") {
              staleConnectionLostEvents.fetch_add(1U, std::memory_order_relaxed);
              break;
            }
          }
          return Value::undefined();
        }));
    const auto failedConnectState = std::make_shared<AppleNativeProtocolExecution::State>(failedConnectControl, nullptr);
    initializeAttachment(failedConnectState, failedConnectInvoker, failedConnectSink);
    const auto failedConnect = connectCommand(1U, "peer-failed-connect", "connection-generation-old", "apple-failed-connect-");
    const auto newConnect = connectCommand(2U, "peer-failed-connect", "connection-generation-new", "apple-new-connect-");
    failedConnectControl->registerCommand(failedConnect, true);
    failedConnectControl->registerCommand(newConnect, true);
    {
      std::scoped_lock lock(failedConnectState->mutex);
      failedConnectState->pendingDisconnects.emplace(
          "peer-failed-connect",
          AppleNativeProtocolExecution::State::PendingDisconnect{
              .attachmentGeneration = failedConnectState->attachmentGeneration,
              .ordinal = 71U,
              .error = std::nullopt,
          });
    }
    fail(failedConnectState, failedConnect, "connectFailed", nil);
    {
      std::scoped_lock lock(failedConnectState->mutex);
      if (!require(
              failedConnectState->pendingDisconnects.empty(),
              "failed connect terminal left a pending pre-ownership disconnect behind")) return 1;
    }
    while (failedConnectInvoker->pending() != 0U) failedConnectInvoker->flushOne();
    static_cast<void>(success(failedConnectState, newConnect));
    while (failedConnectInvoker->pending() != 0U) failedConnectInvoker->flushOne();
    if (!require(
            staleConnectionLostEvents.load(std::memory_order_relaxed) == 0U,
            "failed connect replayed a stale connectionLost event into a new generation")) return 1;
    failedConnectState->closed.store(true, std::memory_order_release);

    const auto throwingControl = openedRuntime();
    const auto throwingState = std::make_shared<AppleNativeProtocolExecution::State>(throwingControl, nullptr);
    std::atomic<std::size_t> fatalDelivered{0U};
    const auto fatalSink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "appleExecutionFatalSink"),
        1U,
        [&fatalDelivered](Runtime&, const Value&, const Value* arguments, std::size_t count) {
          if (count == 1U && arguments[0].isString()) {
            fatalDelivered.fetch_add(1U, std::memory_order_relaxed);
          }
          return Value::undefined();
        }));
    const auto throwingSink = std::make_shared<Function>(Function::createFromHostFunction(
        *runtime,
        PropNameID::forUtf8(*runtime, "throwingAppleExecutionSink"),
        1U,
        [](Runtime& inner, const Value&, const Value*, std::size_t) {
          throw JSError(inner, "controllable Apple sink failure");
          return Value::undefined();
        }));
    initializeAttachment(throwingState, invoker, throwingSink);
    BinaryRuntime binaryRuntime(throwingState);
    auto fatalSetterValue = binaryRuntime.get(
        *runtime,
        PropNameID::forUtf8(*runtime, "setFatalSink"));
    if (!require(
            fatalSetterValue.isObject() && fatalSetterValue.asObject(*runtime).isFunction(*runtime),
            "Apple binary runtime did not expose setFatalSink")) return 1;
    auto fatalSetter = fatalSetterValue.asObject(*runtime).asFunction(*runtime);
    fatalSetter.call(*runtime, *fatalSink);
    const auto sinkFailureCommand = command(1U);
    throwingControl->registerCommand(sinkFailureCommand, true);
    static_cast<void>(success(throwingState, sinkFailureCommand));
    invoker->flushOne();
    if (!require(!throwingControl->open(), "actual JSI sink failure did not fatally close the attachment")) return 1;
    if (!require(invoker->pending() == 1U, "Apple fatal path did not schedule exactly one JavaScript fatal callback")) return 1;
    invoker->flushOne();
    if (!require(fatalDelivered.load(std::memory_order_relaxed) == 1U, "Apple fatal sink was not invoked exactly once")) return 1;
    if (!require(
            invoker->pending() == 0U &&
            enumerateInvoker->pending() == 0U &&
            immediateDisconnectInvoker->pending() == 0U &&
            failedConnectInvoker->pending() == 0U,
            "Apple harness retained a CallInvoker callback before runtime teardown")) return 1;

    // Direct-State seams intentionally bypass AppleNativeProtocolExecution::close().
    // Release their JSI functions while the runtime is still alive, and remove
    // any installed host object before JSCRuntime performs its dangling-object
    // assertion during destruction.
    releaseHarnessJsiState(state);
    releaseHarnessJsiState(missingInvokerState);
    releaseHarnessJsiState(failedConnectState);
    releaseHarnessJsiState(throwingState);
    if (!runtime->global().getProperty(*runtime, "__unifiedBleNativeProtocolV2").isUndefined()) {
      runtime->global().deleteProperty(*runtime, "__unifiedBleNativeProtocolV2");
    }
    return 0;
    }();
    }
    // Deleting the installed global only makes its HostObject graph
    // unreachable. Force JSC to finalize that graph while the runtime is still
    // valid so any API wrappers retained by native-state finalizers are
    // released before JSCRuntime checks its debug object counter.
    runtime->instrumentation().collectGarbage("Apple native protocol harness teardown");
    runtime.reset();
    return harnessResult;
  } catch (const std::exception& error) {
    std::cerr << "Apple Native Protocol execution harness failed: " << error.what() << '\n';
    return 1;
  }
}

} // namespace unified_ble::apple_protocol

int main() {
  @autoreleasepool {
    return unified_ble::apple_protocol::runAppleNativeProtocolExecutionHarness();
  }
}
