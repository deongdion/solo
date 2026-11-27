/* The part of bionic's signal.h and setjmp.h the fixture uses, with bionic's layouts. */
#pragma once

typedef unsigned long sigset_t;
typedef struct siginfo { int si_signo; int si_errno; int si_code; } siginfo_t;

#if defined(__LP64__)
struct sigaction {
    int sa_flags;
    union {
        void (*sa_handler)(int);
        void (*sa_sigaction)(int, siginfo_t*, void*);
    };
    sigset_t sa_mask;
    void (*sa_restorer)(void);
};
#else
struct sigaction {
    union {
        void (*sa_handler)(int);
        void (*sa_sigaction)(int, siginfo_t*, void*);
    };
    sigset_t sa_mask;
    int sa_flags;
    void (*sa_restorer)(void);
};
#endif

#define SA_SIGINFO 4
#define SIGSEGV 11
#define SIGUSR1 10
#define SIGUSR2 12
#define SIG_BLOCK 0
#define SIG_UNBLOCK 1

typedef long sigjmp_buf[65];

int sigaction(int signal, const struct sigaction* action, struct sigaction* old);
int raise(int signal);
int sigprocmask(int how, const sigset_t* set, sigset_t* old);
int sigpending(sigset_t* set);
int sigemptyset(sigset_t* set);
int sigaddset(sigset_t* set, int signal);
int sigismember(const sigset_t* set, int signal);
int sigsetjmp(sigjmp_buf env, int save_mask);
void siglongjmp(sigjmp_buf env, int value);
