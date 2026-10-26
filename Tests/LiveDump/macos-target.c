#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#define COUNT (4 * 1024 * 1024 / sizeof(uint64_t))
static _Atomic uint64_t *values;

static void *write_values(void *unused) {
    (void)unused;
    uint64_t round = 1;
    for (;;) {
        ++round;
        for (size_t i = 0; i < COUNT; ++i) atomic_store_explicit(&values[i], round, memory_order_release);
    }
    return NULL;
}

int main(void) {
    values = mmap(NULL, COUNT * sizeof(uint64_t), PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (values == MAP_FAILED) return 1;
    for (size_t i = 0; i < COUNT; ++i) atomic_init(&values[i], 1);
    pthread_t writer;
    if (pthread_create(&writer, NULL, write_values, NULL) != 0) return 1;
    printf("%p %zu\n", (const void *)values, COUNT);
    fflush(stdout);
    char line[16];
    while (fgets(line, sizeof(line), stdin)) {
        printf("alive %llu\n", atomic_load_explicit(&values[0], memory_order_acquire));
        fflush(stdout);
    }
    return 0;
}
