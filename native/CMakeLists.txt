# SPDX-License-Identifier: GPL-3.0-or-later
# The native Hoshidicts engine for host mode, built by build.rs through the cmake
# crate. hachidori_native is the C ABI in bindings.cpp; every static library it
# needs is installed into lib/ so build.rs can link them by name.
cmake_minimum_required(VERSION 3.22.1)
project(hachidori_native LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_C_STANDARD 99)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
# Imported targets such as Iconv::Iconv may name the compiler's own include
# directory; as -isystem that breaks #include_next in libstdc++ (mingw).
set(CMAKE_NO_SYSTEM_FROM_IMPORTED ON)

if(MSVC)
  # The cmake crate replaces CMake's default MSVC flags, /EHsc among them: the engine
  # throws, so unwinding must be on. /Zc:__cplusplus makes __cplusplus report C++23
  # rather than 199711L; utfcpp hides its std::string overloads without it.
  add_compile_options($<$<COMPILE_LANGUAGE:CXX>:/EHsc> $<$<COMPILE_LANGUAGE:CXX>:/Zc:__cplusplus>)
endif()

add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../third_party/hoshidicts hoshidicts EXCLUDE_FROM_ALL)
if(MSVC)
  # GCC/Clang builtins the engine uses (__builtin_add_overflow).
  target_compile_options(hoshidicts PRIVATE "$<$<COMPILE_LANGUAGE:CXX>:/FI${CMAKE_CURRENT_SOURCE_DIR}/msvc_compat.h>")
  # Hoshidicts' zstd_compress_sequences.c override uses <stdatomic.h>, which MSVC only
  # allows in C11 mode with C11 atomics enabled (GCC and Clang accept it at the C99 the
  # tree asks for).
  set_property(TARGET libzstd_static PROPERTY C_STANDARD 11)
  target_compile_options(libzstd_static PRIVATE $<$<COMPILE_LANGUAGE:C>:/experimental:c11atomics>)
endif()

add_library(hachidori_native STATIC bindings.cpp)
# The language-pack headers bindings.cpp reads (hdw_language_load, hdw_segment,
# hdw_deinflect) include unordered_dense's, a header-only library.
target_link_libraries(hachidori_native PRIVATE hoshidicts glaze::glaze unordered_dense::unordered_dense)
# hdw_import reads an archive's title through the engine's private zip reader.
target_include_directories(hachidori_native PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/hoshidicts/src)
if(MSVC)
  target_compile_options(hachidori_native PRIVATE /utf-8)
endif()

set(HACHIDORI_NATIVE_LIBS hachidori_native hoshidicts libzstd_static libdeflate_static utf8proc lzokay gumbo)
install(TARGETS ${HACHIDORI_NATIVE_LIBS} ARCHIVE DESTINATION lib)
