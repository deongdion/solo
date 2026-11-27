// JNI fixture: JNI_OnLoad registers natives, and Native.run drives the JNI functions a typical
// protected library uses, writing every observation into a string it returns. The same binary
// runs under unidbg (JniRun.java) and here (runtime/tests/jni.rs); the strings must be equal.
// Only JNI functions unidbg implements on both architectures are used (see
// unidbg_unsupported_*.txt), and strings stay in the BMP, where modified UTF-8 and UTF-8 agree.

#include <jni.h>

typedef __SIZE_TYPE__ size_t;
int vsnprintf(char*, size_t, const char*, va_list);
size_t strlen(const char*);

static char out[8192];
static size_t used;

static void say(const char* format, ...) {
    va_list args;
    va_start(args, format);
    int n = vsnprintf(out + used, sizeof out - used, format, args);
    va_end(args);
    if (n > 0) used += (size_t)n < sizeof out - used ? (size_t)n : sizeof out - used - 1;
}

static jint call_int_v(JNIEnv* env, jobject obj, jmethodID m, ...) {
    va_list args;
    va_start(args, m);
    jint r = (*env)->CallIntMethodV(env, obj, m, args);
    va_end(args);
    return r;
}

static jlong call_static_long_v(JNIEnv* env, jclass cls, jmethodID m, ...) {
    va_list args;
    va_start(args, m);
    jlong r = (*env)->CallStaticLongMethodV(env, cls, m, args);
    va_end(args);
    return r;
}

static jstring JNICALL run(JNIEnv* env, jclass self, jobject host) {
    used = 0;
    say("version %x\n", (*env)->GetVersion(env));
    jclass hc = (*env)->FindClass(env, "fix/Host");
    say("instance %d %d\n", (*env)->IsInstanceOf(env, host, hc), (*env)->IsInstanceOf(env, self, hc));

    jmethodID add = (*env)->GetStaticMethodID(env, hc, "add", "(IJ)J");
    say("add %lld %lld\n", (long long)(*env)->CallStaticLongMethod(env, hc, add, (jint)-7, (jlong)0x100000000LL),
        (long long)call_static_long_v(env, hc, add, (jint)40, (jlong)2));

    jmethodID mix = (*env)->GetMethodID(env, hc, "mix", "(ZBCSIJFD)I");
    jint m1 = (*env)->CallIntMethod(env, host, mix, (jboolean)1, (jbyte)-2, (jchar)0x4e2d, (jshort)-300, (jint)70000,
                                    (jlong)-5000000000LL, (jfloat)1.5f, (jdouble)-2.25);
    jint m2 = call_int_v(env, host, mix, (jboolean)0, (jbyte)127, (jchar)65, (jshort)32767, (jint)-1, (jlong)1,
                         (jfloat)-0.5f, (jdouble)1e10);
    jvalue v[8];
    v[0].j = 0; v[1].j = 0; v[2].j = 0; v[3].j = 0;
    v[0].z = 1; v[1].b = 3; v[2].c = 0xffff; v[3].s = -1;
    v[4].i = 123456; v[5].j = 0x7fffffffffffLL; v[6].f = 3.0f; v[7].d = 0.125;
    jint m3 = (*env)->CallIntMethodA(env, host, mix, v);
    say("mix %d %d %d\n", m1, m2, m3);

    jmethodID scale = (*env)->GetMethodID(env, hc, "scale", "(FD)D");
    say("scale %.6f\n", (*env)->CallDoubleMethod(env, host, scale, (jfloat)2.5f, (jdouble)-1.25));

    jstring s = (*env)->NewStringUTF(env, "h\xc3\xa9llo \xed\x95\x9c");
    say("string len %d utf %d\n", (*env)->GetStringLength(env, s), (*env)->GetStringUTFLength(env, s));
    jmethodID echo = (*env)->GetMethodID(env, hc, "echo", "(Ljava/lang/String;)Ljava/lang/String;");
    jstring e = (jstring)(*env)->CallObjectMethod(env, host, echo, s);
    const char* chars = (*env)->GetStringUTFChars(env, e, 0);
    say("echo [%s] %d\n", chars, (int)strlen(chars));
    (*env)->ReleaseStringUTFChars(env, e, chars);
    jchar units[3];
    (*env)->GetStringRegion(env, e, 1, 3, units);
    say("region %x %x %x\n", units[0], units[1], units[2]);

    jbyteArray bytes = (*env)->NewByteArray(env, 6);
    jbyte fill[6] = {1, 2, 3, -4, -5, 127};
    (*env)->SetByteArrayRegion(env, bytes, 0, 6, fill);
    jmethodID reverse = (*env)->GetMethodID(env, hc, "reverse", "([B)[B");
    jbyteArray reversed = (jbyteArray)(*env)->CallObjectMethod(env, host, reverse, bytes);
    jbyte* r = (*env)->GetByteArrayElements(env, reversed, 0);
    say("reverse %d:", (*env)->GetArrayLength(env, reversed));
    for (int i = 0; i < 6; i++) say(" %d", r[i]);
    r[0] = 99;
    (*env)->ReleaseByteArrayElements(env, reversed, r, 0);
    jbyte back[2];
    (*env)->GetByteArrayRegion(env, reversed, 0, 2, back);
    say("\nreleased %d %d\n", back[0], back[1]);

    jintArray ints = (*env)->NewIntArray(env, 4);
    jint* ip = (*env)->GetIntArrayElements(env, ints, 0);
    for (int i = 0; i < 4; i++) ip[i] = (i + 1) * 1000;
    (*env)->ReleaseIntArrayElements(env, ints, ip, 0);
    jint* crit = (jint*)(*env)->GetPrimitiveArrayCritical(env, ints, 0);
    say("ints %d %d\n", crit[0], crit[3]);
    (*env)->ReleasePrimitiveArrayCritical(env, ints, crit, 2);

    jclass string_class = (*env)->FindClass(env, "java/lang/String");
    jobjectArray names = (*env)->NewObjectArray(env, 3, string_class, 0);
    (*env)->SetObjectArrayElement(env, names, 1, s);
    jobject got = (*env)->GetObjectArrayElement(env, names, 1);
    say("object array %d %d %d\n", (*env)->GetArrayLength(env, names), (*env)->IsSameObject(env, got, s),
        (*env)->GetObjectArrayElement(env, names, 0) == 0);

    jfieldID count = (*env)->GetFieldID(env, hc, "count", "I");
    say("count %d", (*env)->GetIntField(env, host, count));
    (*env)->SetIntField(env, host, count, 41);
    say(" -> %d\n", (*env)->GetIntField(env, host, count));
    jfieldID name = (*env)->GetStaticFieldID(env, hc, "NAME", "Ljava/lang/String;");
    jstring n = (jstring)(*env)->GetStaticObjectField(env, hc, name);
    const char* nc = (*env)->GetStringUTFChars(env, n, 0);
    say("static NAME [%s]\n", nc);
    (*env)->ReleaseStringUTFChars(env, n, nc);

    jmethodID ctor = (*env)->GetMethodID(env, hc, "<init>", "(I)V");
    jobject made = (*env)->NewObject(env, hc, ctor, (jint)5);
    say("new %d count %d\n", made != 0, (*env)->GetIntField(env, made, count));

    jmethodID boom = (*env)->GetMethodID(env, hc, "boom", "()V");
    (*env)->CallVoidMethod(env, host, boom);
    jthrowable t = (*env)->ExceptionOccurred(env);
    say("exception %d", (*env)->ExceptionCheck(env));
    (*env)->ExceptionClear(env);
    say(" cleared %d", (*env)->ExceptionCheck(env));
    (*env)->Throw(env, t);
    say(" rethrown %d", (*env)->ExceptionCheck(env));
    (*env)->ExceptionClear(env);
    say(" %d\n", (*env)->ExceptionCheck(env));

    jobject g = (*env)->NewGlobalRef(env, host);
    jweak w = (*env)->NewWeakGlobalRef(env, host);
    say("refs %d %d %d %d\n", (*env)->IsSameObject(env, g, host), (*env)->GetObjectRefType(env, g),
        (*env)->GetObjectRefType(env, host), (*env)->GetObjectRefType(env, w));
    (*env)->DeleteGlobalRef(env, g);
    (*env)->PushLocalFrame(env, 16);
    jstring inner = (*env)->NewStringUTF(env, "kept");
    jstring kept = (jstring)(*env)->PopLocalFrame(env, inner);
    const char* kc = (*env)->GetStringUTFChars(env, kept, 0);
    say("frame [%s]\n", kc);
    (*env)->ReleaseStringUTFChars(env, kept, kc);

    // A method that is itself a registered native.
    jmethodID sum = (*env)->GetStaticMethodID(env, self, "sum", "(IJFD)D");
    say("sum via JNI %.4f\n", (*env)->CallStaticDoubleMethod(env, self, sum, (jint)1, (jlong)2, (jfloat)0.5f, (jdouble)0.25));

    JavaVM* vm = 0;
    (*env)->GetJavaVM(env, &vm);
    JNIEnv* again = 0;
    jint ok = (*vm)->GetEnv(vm, (void**)&again, JNI_VERSION_1_6);
    say("vm %d same env %d\n", ok, again == env);
    return (*env)->NewStringUTF(env, out);
}

static jdouble JNICALL sum(JNIEnv* env, jclass self, jint a, jlong b, jfloat c, jdouble d) {
    return (jdouble)a * 1000.0 + (jdouble)b * 100.0 + (jdouble)c * 10.0 + d;
}

JNIEXPORT jstring JNICALL Java_fix_Native_hello(JNIEnv* env, jclass self, jstring who) {
    const char* w = (*env)->GetStringUTFChars(env, who, 0);
    used = 0;
    say("hello %s", w);
    (*env)->ReleaseStringUTFChars(env, who, w);
    return (*env)->NewStringUTF(env, out);
}

JNIEXPORT jint JNICALL Java_fix_Native_twice__I(JNIEnv* env, jclass self, jint x) {
    return x * 2;
}

JNIEXPORT jint JNI_OnLoad(JavaVM* vm, void* reserved) {
    JNIEnv* env;
    if ((*vm)->GetEnv(vm, (void**)&env, JNI_VERSION_1_6) != JNI_OK) return -1;
    jclass c = (*env)->FindClass(env, "fix/Native");
    JNINativeMethod methods[] = {
        {"run", "(Lfix/Host;)Ljava/lang/String;", (void*)run},
        {"sum", "(IJFD)D", (void*)sum},
    };
    if ((*env)->RegisterNatives(env, c, methods, 2) != 0) return -2;
    return JNI_VERSION_1_6;
}

// Walks a java.util.Map through the interfaces, as natives reading request parameters do.
JNIEXPORT jstring JNICALL Java_fix_Native_walk(JNIEnv* env, jclass self, jobject map) {
    used = 0;
    jclass map_class = (*env)->FindClass(env, "java/util/Map");
    jclass set_class = (*env)->FindClass(env, "java/util/Set");
    jclass iterator_class = (*env)->FindClass(env, "java/util/Iterator");
    jclass integer_class = (*env)->FindClass(env, "java/lang/Integer");
    jmethodID size = (*env)->GetMethodID(env, map_class, "size", "()I");
    jmethodID key_set = (*env)->GetMethodID(env, map_class, "keySet", "()Ljava/util/Set;");
    jmethodID get = (*env)->GetMethodID(env, map_class, "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    jmethodID iterator = (*env)->GetMethodID(env, set_class, "iterator", "()Ljava/util/Iterator;");
    jmethodID has_next = (*env)->GetMethodID(env, iterator_class, "hasNext", "()Z");
    jmethodID next = (*env)->GetMethodID(env, iterator_class, "next", "()Ljava/lang/Object;");
    jmethodID int_value = (*env)->GetMethodID(env, integer_class, "intValue", "()I");
    say("size %d:", (*env)->CallIntMethod(env, map, size));
    jobject keys = (*env)->CallObjectMethod(env, map, key_set);
    jobject it = (*env)->CallObjectMethod(env, keys, iterator);
    while ((*env)->CallBooleanMethod(env, it, has_next)) {
        jstring key = (jstring)(*env)->CallObjectMethod(env, it, next);
        jobject value = (*env)->CallObjectMethod(env, map, get, key);
        const char* k = (*env)->GetStringUTFChars(env, key, 0);
        say(" %s=%d", k, (*env)->CallIntMethod(env, value, int_value));
        (*env)->ReleaseStringUTFChars(env, key, k);
    }
    say("\n");
    return (*env)->NewStringUTF(env, out);
}

// Spins for `rounds` iterations (a negative count never returns); for deadline tests.
JNIEXPORT jint JNICALL Java_fix_Native_spin(JNIEnv* env, jclass self, jint rounds) {
    volatile jint acc = 0;
    for (jint i = 0; rounds < 0 || i < rounds; i++) acc += i;
    return acc;
}

// Threads: workers adding under a mutex, two threads alternating through a condition variable,
// and a thread that only spins, which runs only if the scheduler preempts the spinning caller.
#include <pthread.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t turn_changed = PTHREAD_COND_INITIALIZER;
static int counter, turn, pings, rounds_each;
static volatile int go, done;

struct work { int add, rounds; };

static void* adder(void* p) {
    struct work* w = (struct work*)p;
    for (int i = 0; i < w->rounds; i++) {
        pthread_mutex_lock(&lock);
        counter += w->add;
        pthread_mutex_unlock(&lock);
    }
    return (void*)(long)(w->add * 10);
}

static void* player(void* p) {
    int me = (int)(long)p;
    for (int i = 0; i < rounds_each; i++) {
        pthread_mutex_lock(&lock);
        while (turn != me) pthread_cond_wait(&turn_changed, &lock);
        pings++;
        turn = 1 - me;
        pthread_cond_broadcast(&turn_changed);
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

static void* spinner(void* p) {
    while (!go) {}
    done = 1;
    return p;
}

// parts: 1 adders, 2 ping-pong, 4 spinner.
JNIEXPORT jstring JNICALL Java_fix_Native_threads(JNIEnv* env, jclass self, jint n, jint rounds, jint parts) {
    used = 0;
    if (parts & 1) {
        counter = 0;
        pthread_t threads[8];
        struct work work[8];
        long joined = 0;
        if (n > 8) n = 8;
        for (int i = 0; i < n; i++) {
            work[i].add = i + 1;
            work[i].rounds = rounds;
            if (pthread_create(&threads[i], 0, adder, &work[i]) != 0) return (*env)->NewStringUTF(env, "create failed");
        }
        for (int i = 0; i < n; i++) {
            void* r = 0;
            pthread_join(threads[i], &r);
            joined += (long)r;
        }
        say("counter %d joined %ld;", counter, joined);
    }
    if (parts & 2) {
        turn = 0;
        pings = 0;
        rounds_each = rounds;
        pthread_t a, b;
        pthread_create(&a, 0, player, (void*)0L);
        pthread_create(&b, 0, player, (void*)1L);
        pthread_join(a, 0);
        pthread_join(b, 0);
        say(" pings %d turn %d;", pings, turn);
    }
    if (parts & 4) {
        go = 0;
        done = 0;
        pthread_t s;
        pthread_create(&s, 0, spinner, (void*)7L);
        go = 1;
        while (!done) {}
        void* r = 0;
        pthread_join(s, &r);
        say(" spun %ld;", (long)r);
    }
    say("\n");
    return (*env)->NewStringUTF(env, out);
}
