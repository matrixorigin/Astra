#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

/* Test-only fault: preserve every other file and stdout, including earlier
 * stream evidence. The production CLI has no fault-injection switch. */
static int is_event_file(int fd) {
    char link[64], path[4096];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, sizeof(path) - 1);
    if (n < 0) return 0;
    path[n] = '\0';
    const char *suffix = "/events.jsonl";
    size_t len = strlen(suffix);
    return (size_t)n >= len && strcmp(path + n - len, suffix) == 0;
}

static int has_fault_marker(const void *data, size_t len) {
    const char *marker = "force-writer-enospc";
    return memmem(data, len, marker, strlen(marker)) != NULL;
}

ssize_t write(int fd, const void *data, size_t len) {
    if (is_event_file(fd) && has_fault_marker(data, len)) {
        errno = ENOSPC;
        return -1;
    }
    ssize_t (*next)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    return next(fd, data, len);
}

ssize_t writev(int fd, const struct iovec *iov, int count) {
    if (is_event_file(fd)) {
        for (int i = 0; i < count; ++i) {
            if (has_fault_marker(iov[i].iov_base, iov[i].iov_len)) {
                errno = ENOSPC;
                return -1;
            }
        }
    }
    ssize_t (*next)(int, const struct iovec *, int) = dlsym(RTLD_NEXT, "writev");
    return next(fd, iov, count);
}
