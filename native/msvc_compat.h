// SPDX-License-Identifier: GPL-3.0-or-later
// Force-included into the Hoshidicts sources when they are built with MSVC (see
// CMakeLists.txt), which lacks the GCC/Clang builtins they use. Upstream builds them
// with GCC and Clang only.
#pragma once

#if defined(_MSC_VER) && !defined(__clang__) && defined(__cplusplus)
#include <type_traits>

// Only the unsigned form is used (json_skip.cpp's carry out of a 64-bit add), where
// wrap-around is defined and a carry shows as a sum smaller than an operand.
template <class T>
constexpr bool __builtin_add_overflow(T a, T b, T* sum) noexcept {
  static_assert(std::is_unsigned_v<T>, "MSVC shim covers unsigned operands only");
  *sum = static_cast<T>(a + b);
  return *sum < a;
}
#endif
