/* The part of bionic's pthread.h the fixture uses, with bionic's type layouts. */
#pragma once

#include <stddef.h>

typedef long pthread_t;
typedef struct pthread_attr_t pthread_attr_t;

#if defined(__LP64__)
typedef struct { int __private[10]; } pthread_mutex_t;
typedef struct { int __private[12]; } pthread_cond_t;
#else
typedef struct { int __private[1]; } pthread_mutex_t;
typedef struct { int __private[1]; } pthread_cond_t;
#endif

#define PTHREAD_MUTEX_INITIALIZER { { 0 } }
#define PTHREAD_COND_INITIALIZER { { 0 } }

int pthread_create(pthread_t* thread, const pthread_attr_t* attr, void* (*start)(void*), void* arg);
int pthread_join(pthread_t thread, void** result);
int pthread_mutex_lock(pthread_mutex_t* mutex);
int pthread_mutex_unlock(pthread_mutex_t* mutex);
int pthread_cond_wait(pthread_cond_t* cond, pthread_mutex_t* mutex);
int pthread_cond_broadcast(pthread_cond_t* cond);
int sched_yield(void);
