/* zig builds its bundled libunwind for ppc64le with the IEEE-128 long double ABI, which
 * references glibc 2.32+ symbols. RHEL 8 has glibc 2.28. libunwind only uses these for
 * diagnostics and never with long double arguments, so the plain versions are equivalent. */
#include <stdarg.h>
#include <stdio.h>

int __fprintfieee128(FILE *stream, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(stream, fmt, ap);
    va_end(ap);
    return r;
}

int __snprintfieee128(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return r;
}
