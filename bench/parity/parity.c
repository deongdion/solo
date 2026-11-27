// Parity fixture: exercises bionic through deterministic calls and records every result.
// The same binary runs under unidbg and under this project; the text parity_run writes into the
// caller's buffer must match byte for byte. (unidbg mixes its own logging into System.out, so the
// results do not go through stdout.)
// Built without NDK headers, so the few declarations needed are written out here.

typedef __SIZE_TYPE__ size_t;
typedef long time_t;
struct tm {
    int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst;
    long tm_gmtoff;
    const char* tm_zone;
};

typedef __builtin_va_list va_list;
int vsnprintf(char*, size_t, const char*, va_list);
int snprintf(char*, size_t, const char*, ...);
int sscanf(const char*, const char*, ...);
size_t strlen(const char*);
int strcmp(const char*, const char*);
int strncmp(const char*, const char*, size_t);
char* strstr(const char*, const char*);
char* strchr(const char*, int);
char* strrchr(const char*, int);
char* strcpy(char*, const char*);
char* strcat(char*, const char*);
char* strdup(const char*);
int memcmp(const void*, const void*, size_t);
void* memset(void*, int, size_t);
void* memcpy(void*, const void*, size_t);
void* memmove(void*, const void*, size_t);
long strtol(const char*, char**, int);
unsigned long strtoul(const char*, char**, int);
double strtod(const char*, char**);
int atoi(const char*);
void qsort(void*, size_t, size_t, int (*)(const void*, const void*));
void* bsearch(const void*, const void*, size_t, size_t, int (*)(const void*, const void*));
void srand(unsigned);
int rand(void);
void* malloc(size_t);
void* calloc(size_t, size_t);
void* realloc(void*, size_t);
void free(void*);
char* getenv(const char*);
int* __errno(void);
char* strerror(int);
int open(const char*, int, ...);
int close(int);
struct tm* gmtime_r(const time_t*, struct tm*);
size_t strftime(char*, size_t, const char*, const struct tm*);
time_t timegm(struct tm*);
void* dlopen(const char*, int);
void* dlsym(void*, const char*);
char* dlerror(void);
int toupper(int);
int isdigit(int);

static char* out_buffer;
static size_t out_size, out_length;

static void printf(const char* format, ...) {
    va_list args;
    __builtin_va_start(args, format);
    size_t room = out_length < out_size ? out_size - out_length : 0;
    int n = vsnprintf(out_buffer + out_length, room, format, args);
    __builtin_va_end(args);
    if (n > 0) out_length += (size_t)n < room ? (size_t)n : (room ? room - 1 : 0);
}

static int compare(const void* a, const void* b) {
    int x = *(const int*)a, y = *(const int*)b;
    return (x > y) - (x < y);
}

#ifndef PARITY_NO_DATA_RELOCATIONS
// An exported pointer table in relro gives the library a .rel.dyn (R_*_RELATIVE) section.
int (*const parity_comparators[])(const void*, const void*) = {compare};
#define COMPARE parity_comparators[0]
#else
#define COMPARE compare
#endif

static void formats(void) {
    printf("int: %d %i %u %x %X %o\n", -42, 42, 4000000000u, 0xbeef, 0xbeef, 8);
    printf("width: [%5d] [%-5d] [%05d] [%+d] [% d]\n", 42, 42, 42, 42, 42);
    printf("long: %ld %lu %lld %llu %llx\n", -1L, 1UL << 31, -9000000000LL, 18000000000ULL, 0x123456789abcULL);
    printf("short char: %hd %hhu %c %%\n", (short)-2, (unsigned char)250, 'Q');
    printf("string: [%s] [%10s] [%-10s] [%.3s]\n", "abc", "abc", "abc", "abcdef");
    printf("float: %f %.2f %10.4f %e %E %g %G\n", 3.14159265358979, 2.5, -1.0 / 3.0, 12345.678, 0.000123, 1e-5, 1e20);
    printf("float special: %g %g %a\n", 0.1 + 0.2, 1.0 / 7.0, 1.0);
    char buf[16];
    int n = snprintf(buf, sizeof buf, "%s-%d-%s", "truncate", 12345, "me");
    printf("snprintf: %d [%s]\n", n, buf);
}

static void strings(void) {
    char a[64];
    strcpy(a, "hello");
    strcat(a, ", world");
    printf("strlen %d strcmp %d %d strncmp %d\n", (int)strlen(a), strcmp(a, "hello") > 0, strcmp("a", "b") < 0,
           strncmp(a, "help", 3));
    printf("strstr [%s] strchr [%s] strrchr [%s]\n", strstr(a, "wor"), strchr(a, 'o'), strrchr(a, 'o'));
    char* d = strdup(a);
    for (char* p = d; *p; p++) *p = (char)toupper(*p);
    printf("upper [%s] memcmp %d\n", d, memcmp(a, d, 5) > 0);
    free(d);
    char m[16] = "0123456789";
    memmove(m + 2, m, 6);
    printf("memmove [%s]\n", m);
    memset(m, 'x', 3);
    printf("memset [%.10s]\n", m);
}

static void numbers(void) {
    char* end;
    long l = strtol("  -12345xyz", &end, 10);
    printf("strtol %ld rest [%s]\n", l, end);
    printf("strtol hex %ld oct %ld base36 %ld\n", strtol("0x1f", 0, 0), strtol("0777", 0, 0), strtol("zz", 0, 36));
    printf("strtoul %lu atoi %d\n", strtoul("4294967295", 0, 10), atoi("2147483647"));
    double v = strtod("6.02214076e23", &end);
    printf("strtod %.10g %.17g\n", v, strtod("0.1", 0));
    int i1, i2;
    char word[16];
    int got = sscanf("12 abc 0x10", "%d %15s %x", &i1, word, &i2);
    printf("sscanf %d %d [%s] %d\n", got, i1, word, i2);
    printf("isdigit %d %d\n", isdigit('7') != 0, isdigit('x') != 0);
}

static void algorithms(void) {
    int values[] = {9, -3, 7, 0, 42, 7, -100, 15};
    qsort(values, 8, sizeof values[0], COMPARE);
    printf("qsort");
    for (int i = 0; i < 8; i++) printf(" %d", values[i]);
    int key = 15;
    int* found = bsearch(&key, values, 8, sizeof values[0], compare);
    printf("\nbsearch index %d\n", found ? (int)(found - values) : -1);
    srand(1);
    printf("rand %d %d %d\n", rand(), rand(), rand());
}

static void time_and_errors(void) {
    time_t t = 1700000000;
    struct tm tm;
    gmtime_r(&t, &tm);
    char buf[64];
    strftime(buf, sizeof buf, "%Y-%m-%d %H:%M:%S %a %j", &tm);
    printf("gmtime %s yday %d wday %d\n", buf, tm.tm_yday, tm.tm_wday);
    printf("timegm %ld\n", (long)timegm(&tm));
    int fd = open("/nonexistent/parity", 0);
    printf("open %d errno %d [%s]\n", fd, *__errno(), strerror(*__errno()));
    printf("strerror 13 [%s]\n", strerror(13));
    printf("getenv ANDROID_ROOT [%s] missing %d\n", getenv("ANDROID_ROOT"), getenv("PARITY_MISSING") == 0);
}

static void heap(void) {
    int* p = calloc(16, sizeof(int));
    int zero = 1;
    for (int i = 0; i < 16; i++) zero &= p[i] == 0;
    for (int i = 0; i < 16; i++) p[i] = i * i;
    p = realloc(p, 1024 * sizeof(int));
    int sum = 0;
    for (int i = 0; i < 16; i++) sum += p[i];
    printf("calloc zero %d realloc kept sum %d\n", zero, sum);
    free(p);
    void* blocks[64];
    for (int i = 0; i < 64; i++) blocks[i] = malloc((size_t)(i * 37 + 1));
    int distinct = 1;
    for (int i = 1; i < 64; i++) distinct &= blocks[i] != blocks[i - 1];
    for (int i = 0; i < 64; i++) free(blocks[i]);
    printf("malloc distinct %d\n", distinct);
}

static void dynamic(void) {
    void* libm = dlopen("libm.so", 0);
    double (*cosine)(double) = libm ? (double (*)(double))dlsym(libm, "cos") : 0;
    printf("dlopen libm %d dlsym cos %d\n", libm != 0, cosine != 0);
    if (cosine) printf("cos(1) %.17g\n", cosine(1.0));
    void* missing = dlsym(libm, "parity_missing_symbol");
    printf("dlsym missing %d dlerror set %d\n", missing == 0, dlerror() != 0);
}

/// Writes the results into `buffer` (NUL-terminated) and returns their length.
int parity_run(char* buffer, size_t size) {
    out_buffer = buffer;
    out_size = size;
    out_length = 0;
    if (size) buffer[0] = 0;
    formats();
    strings();
    numbers();
    algorithms();
    time_and_errors();
    heap();
    dynamic();
    printf("done\n");
    return (int)out_length;
}
