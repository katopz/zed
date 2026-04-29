#include <algorithm>
#include <cstddef>
#include <cstdint>

extern "C" {

const void* __stdcall __std_search_1(
    const void* _First1, const void* _Last1,
    const void* _First2, const void* _Last2) noexcept {
    const auto* f1 = static_cast<const uint8_t*>(_First1);
    const auto* l1 = static_cast<const uint8_t*>(_Last1);
    const auto* f2 = static_cast<const uint8_t*>(_First2);
    const auto* l2 = static_cast<const uint8_t*>(_Last2);
    return std::search(f1, l1, f2, l2);
}

size_t __stdcall __std_find_first_of_trivial_pos_1(
    const void* _First1, const void* _Last1,
    const void* _First2, const void* _Last2) noexcept {
    const auto* f1 = static_cast<const uint8_t*>(_First1);
    const auto* l1 = static_cast<const uint8_t*>(_Last1);
    const auto* f2 = static_cast<const uint8_t*>(_First2);
    const auto* l2 = static_cast<const uint8_t*>(_Last2);
    const auto* result = std::find_first_of(f1, l1, f2, l2);
    if (result == l1) {
        return static_cast<size_t>(-1);
    }
    return static_cast<size_t>(result - f1);
}

const void* __stdcall __std_find_end_1(
    const void* _First1, const void* _Last1,
    const void* _First2, const void* _Last2) noexcept {
    const auto* f1 = static_cast<const uint8_t*>(_First1);
    const auto* l1 = static_cast<const uint8_t*>(_Last1);
    const auto* f2 = static_cast<const uint8_t*>(_First2);
    const auto* l2 = static_cast<const uint8_t*>(_Last2);
    return std::find_end(f1, l1, f2, l2);
}

const void* __stdcall __std_find_end_2(
    const void* _First1, const void* _Last1,
    const void* _First2, const void* _Last2) noexcept {
    const auto* f1 = static_cast<const uint16_t*>(_First1);
    const auto* l1 = static_cast<const uint16_t*>(_Last1);
    const auto* f2 = static_cast<const uint16_t*>(_First2);
    const auto* l2 = static_cast<const uint16_t*>(_Last2);
    return std::find_end(f1, l1, f2, l2);
}

void* __stdcall __std_remove_8(
    void* _First, void* _Last, uint64_t _Val) noexcept {
    auto* f = static_cast<uint64_t*>(_First);
    auto* l = static_cast<uint64_t*>(_Last);
    return std::remove(f, l, _Val);
}

}