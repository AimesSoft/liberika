#pragma once
#define LOG_APP 0
#define LOG_INFO 0
// OHOS %{public} format specifiers are specific to hilog.
inline int OH_LOG_Print(int, int, unsigned, const char*, const char*, ...) { return 0; }
