#pragma once

#if defined(_WIN32) && defined(GREET_BUILD)
#define GREET_API __declspec(dllexport)
#elif defined(_WIN32)
#define GREET_API __declspec(dllimport)
#else
#define GREET_API
#endif

GREET_API void greet(const char* name);
