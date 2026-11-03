/* Private causal diagnostic: enlarge an existing pipe -> fcntl scheduling window. */
#include <unistd.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <fcntl.h>
#include <stdio.h>
#include <errno.h>
#include <pthread.h>
#include <time.h>
static int windowed_pipe(int fds[2]) {
    int result = pipe(fds);
    if (result != 0 || strcmp(getprogname(), "scode") != 0) return result;
    int saved_errno = errno;
    const char *log_path = getenv("SCODE_DIAGNOSTIC_PIPE_WINDOW_LOG");
    if (log_path) {
        int log = open(log_path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
        if (log >= 0) {
            uint64_t thread_id = 0;
            pthread_threadid_np(NULL, &thread_id);
            dprintf(log, "pid=%d thread=%llu pipe=%d,%d cloexec=%d,%d before_real_caller_fcntl\n", getpid(), (unsigned long long)thread_id, fds[0], fds[1], fcntl(fds[0], F_GETFD), fcntl(fds[1], F_GETFD));
            close(log);
        }
    }
    struct timespec remaining = {.tv_sec = 0, .tv_nsec = 100000000};
    while (nanosleep(&remaining, &remaining) != 0 && errno == EINTR) {}
    errno = saved_errno;
    return result;
}
__attribute__((used)) static struct { const void *replacement; const void *replacee; }
interpose_pipe __attribute__((section("__DATA,__interpose"))) = {(const void *)&windowed_pipe, (const void *)&pipe};
