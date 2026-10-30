#include <cstdio>

int main()
{
    std::printf("level = %d\n", APP_LEVEL);
#ifdef ENABLE_TRACE
    std::puts("trace enabled");
#endif
}
