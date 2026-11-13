#include <cstdio>

int main() {
    char line[64] = {};
    FILE* file = std::fopen("assets/greeting.txt", "r");
    if (!file) {
        std::puts("assets/greeting.txt not found");
        return 1;
    }
    std::fgets(line, sizeof line, file);
    std::fclose(file);
#ifdef __EMSCRIPTEN__
    std::printf("webassembly: %s", line);
#else
    std::printf("native: %s", line);
#endif
    return 0;
}
