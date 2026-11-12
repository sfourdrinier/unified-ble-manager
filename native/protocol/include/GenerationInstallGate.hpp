// native/protocol/include/GenerationInstallGate.hpp
//
// A host-neutral admission gate for JavaScript-runtime installation work.
// Replacing or invalidating an owner makes future queued callbacks fail closed,
// while an already admitted callback finishes before native state can move on.

#pragma once

#include <condition_variable>
#include <cstdint>
#include <limits>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <unordered_map>
#include <utility>
#include <vector>

namespace unified_ble::native_protocol::v2 {

class GenerationInstallGate final {
 public:
  struct Ticket final {
    std::int32_t ownerId;
    std::uint64_t generation;

    friend bool operator==(const Ticket&, const Ticket&) = default;
  };

  [[nodiscard]] Ticket replace(std::int32_t ownerId) {
    return replaceWhenIdle(ownerId, [] {});
  }

  // Publishes the matching native state while replacement is still serialized.
  // The callback must perform no JavaScript work.
  template <typename Callback>
  [[nodiscard]] Ticket replaceWhenIdle(std::int32_t ownerId, Callback&& callback) {
    std::unique_lock lock(mutex_);
    rejectReentrantTransition(ownerId);
    if (nextGeneration_ == std::numeric_limits<std::uint64_t>::max()) {
      throw std::overflow_error("JSI installation generation exhausted");
    }
    const auto entry = transitionEntry(lock, ownerId);
    entry->transitioning = true;
    const Ticket ticket{ownerId, ++nextGeneration_};
    entry->currentGeneration = ticket.generation;
    entry->active = true;

    // A selection change is native-only. Once it invalidates older tickets,
    // wait for any already-admitted JSI write before replacing its state map.
    // The mutex is not retained across that JavaScript write.
    idle_.wait(lock, [&] { return entry->inFlight == 0U; });
    std::forward<Callback>(callback)();
    entry->transitioning = false;
    idle_.notify_all();
    return ticket;
  }

  [[nodiscard]] bool isCurrent(const Ticket& ticket) const {
    std::scoped_lock lock(mutex_);
    const auto found = current_.find(ticket.ownerId);
    return found != current_.end() && found->second->active &&
        !found->second->transitioning && found->second->currentGeneration == ticket.generation;
  }

  // Runs an admitted write without retaining a native mutex while JavaScript
  // property semantics run. Replacement and invalidation first make future
  // tickets stale, then wait for this permit before completing their state
  // transition.
  template <typename Callback>
  [[nodiscard]] bool withCurrent(const Ticket& ticket, Callback&& callback) {
    if (!admit(ticket)) return false;
    struct Completion final {
      GenerationInstallGate& gate;
      Ticket ticket;
      Completion(GenerationInstallGate& gateValue, Ticket ticketValue)
          : gate(gateValue), ticket(ticketValue) {
        admissions_.push_back({&gate, ticket.ownerId});
      }
      ~Completion() {
        gate.complete(ticket);
        admissions_.pop_back();
      }
    } completion{*this, ticket};
    std::forward<Callback>(callback)();
    return true;
  }

  // Fences reads synchronously, then waits for any admitted installation
  // before the state is eligible for reuse. The callback runs gate->state and
  // must not invoke JavaScript.
  template <typename Callback>
  [[nodiscard]] bool invalidateWhenIdle(std::int32_t ownerId, Callback&& callback) {
    std::unique_lock lock(mutex_);
    rejectReentrantTransition(ownerId);
    const auto found = current_.find(ownerId);
    if (found == current_.end()) return false;
    const auto entry = found->second;
    idle_.wait(lock, [&] { return !entry->transitioning; });
    const auto current = current_.find(ownerId);
    if (current == current_.end() || current->second != entry) return false;
    entry->transitioning = true;
    entry->active = false;
    std::forward<Callback>(callback)();
    idle_.wait(lock, [&] { return entry->inFlight == 0U; });
    current_.erase(ownerId);
    entry->transitioning = false;
    idle_.notify_all();
    return true;
  }

 private:
  struct Entry final {
    std::uint64_t currentGeneration{0U};
    std::size_t inFlight{0U};
    bool active{false};
    bool transitioning{false};
  };

  struct ActiveAdmission final {
    const GenerationInstallGate* gate;
    std::int32_t ownerId;
  };

  static bool isAdmittedByCurrentThread(const GenerationInstallGate* gate, std::int32_t ownerId) {
    for (const auto& admission : admissions_) {
      if (admission.gate == gate && admission.ownerId == ownerId) return true;
    }
    return false;
  }

  void rejectReentrantTransition(std::int32_t ownerId) const {
    if (isAdmittedByCurrentThread(this, ownerId)) {
      throw std::logic_error(
          "cannot replace or invalidate a RustCore JSI installation from its own JavaScript write");
    }
  }

  [[nodiscard]] std::shared_ptr<Entry> transitionEntry(
      std::unique_lock<std::mutex>& lock, std::int32_t ownerId) {
    for (;;) {
      const auto found = current_.find(ownerId);
      if (found == current_.end()) {
        const auto created = std::make_shared<Entry>();
        current_.emplace(ownerId, created);
        return created;
      }
      const auto entry = found->second;
      idle_.wait(lock, [&] { return !entry->transitioning; });
      const auto current = current_.find(ownerId);
      if (current != current_.end() && current->second == entry) return entry;
    }
  }

  [[nodiscard]] bool admit(const Ticket& ticket) {
    std::scoped_lock lock(mutex_);
    const auto found = current_.find(ticket.ownerId);
    if (found == current_.end() || !found->second->active || found->second->transitioning ||
        found->second->currentGeneration != ticket.generation) {
      return false;
    }
    ++found->second->inFlight;
    return true;
  }

  void complete(const Ticket& ticket) {
    std::scoped_lock lock(mutex_);
    const auto found = current_.find(ticket.ownerId);
    if (found == current_.end() || found->second->inFlight == 0U) {
      throw std::logic_error("JSI installation completion lost its admission");
    }
    --found->second->inFlight;
    if (found->second->inFlight == 0U) idle_.notify_all();
  }

  mutable std::mutex mutex_;
  std::condition_variable idle_;
  std::unordered_map<std::int32_t, std::shared_ptr<Entry>> current_;
  std::uint64_t nextGeneration_{0U};
  inline static thread_local std::vector<ActiveAdmission> admissions_;
};

}  // namespace unified_ble::native_protocol::v2
