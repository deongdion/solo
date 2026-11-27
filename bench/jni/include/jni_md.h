/* Platform part of jni.h for Android targets (the JDK copy is for the build host). */
#ifndef JNI_MD_ANDROID_H
#define JNI_MD_ANDROID_H
#define JNIEXPORT __attribute__((visibility("default")))
#define JNIIMPORT
#define JNICALL
typedef int jint;
typedef long long jlong;
typedef signed char jbyte;
#endif
