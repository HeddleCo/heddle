#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int rename(const char *source, const char *destination) {
    static int (*real_rename)(const char *, const char *);
    static int failed;
    if (real_rename == NULL) {
        real_rename = dlsym(RTLD_NEXT, "rename");
    }
    const char *lane = getenv("HEDDLE_TEST_FAIL_INDEX");
    if (!failed && lane != NULL && strstr(destination, lane) != NULL &&
        strstr(destination, "/.git/index") != NULL) {
        failed = 1;
        fputs("INJECTED_INDEX_FAILURE\n", stderr);
        errno = EIO;
        return -1;
    }
    return real_rename(source, destination);
}
