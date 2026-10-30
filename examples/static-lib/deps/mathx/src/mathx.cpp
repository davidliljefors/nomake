#include <mathx.h>

namespace mathx
{
int gcd(int a, int b)
{
    while (b != 0)
    {
        int rest = a % b;
        a = b;
        b = rest;
    }
    return a;
}

int lcm(int a, int b)
{
    return a / gcd(a, b) * b;
}
}
