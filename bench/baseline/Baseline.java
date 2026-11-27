import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.VM;
import com.github.unidbg.linux.android.dvm.array.ByteArray;
import com.github.unidbg.linux.android.dvm.jni.ProxyClassFactory;
import com.github.unidbg.memory.Memory;
import com.github.unidbg.virtualmodule.android.AndroidModule;
import com.github.unidbg.virtualmodule.android.JniGraphics;

import java.io.File;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

/**
 * M0 baseline: per-call latency, init time and throughput of unidbg on libtmessages.29.so.
 * Usage: Baseline <unicorn2|dynarmic> <arm64|arm32> <latency|throughput> [instances] [seconds]
 */
public class Baseline {

    static final String LIBS = "src/test/resources/example_binaries/";

    final AndroidEmulator emulator;
    final VM vm;
    final DvmClass utilities;

    Baseline(String backend, boolean is64) {
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.setProcessName("org.telegram.messenger");
        if (backend.equals("dynarmic")) {
            builder.addBackendFactory(new DynarmicFactory(false));
        } else {
            builder.addBackendFactory(new Unicorn2Factory(false));
        }
        emulator = builder.build();
        Memory memory = emulator.getMemory();
        memory.setLibraryResolver(new AndroidResolver(23));
        vm = emulator.createDalvikVM();
        vm.setDvmClassFactory(new ProxyClassFactory());
        vm.setVerbose(false);
        new JniGraphics(emulator, vm).register(memory);
        new AndroidModule(emulator, vm).register(memory);
        File file = new File(LIBS + (is64 ? "arm64-v8a" : "armeabi-v7a") + "/libtmessages.29.so");
        DalvikModule dm = vm.loadLibrary(file, true);
        dm.callJNI_OnLoad(emulator);
        utilities = vm.resolveClass("org/telegram/messenger/Utilities");
    }

    byte[] aesCbc() {
        ByteArray data = new ByteArray(vm, new byte[16]);
        utilities.callStaticJniMethod(emulator, "aesCbcEncryptionByteArray([B[B[BIIII)V",
                data, new byte[32], new byte[16], 0, 16, 0, 0);
        return data.getValue();
    }

    byte[] pbkdf2(int iterations) {
        ByteArray dst = new ByteArray(vm, new byte[64]);
        utilities.callStaticJniMethod(emulator, "pbkdf2([B[B[BI)V",
                "123456".getBytes(), new byte[8], dst, iterations);
        return dst.getValue();
    }

    static String sha(byte[] b) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(b)).substring(0, 16);
    }

    static String stats(long[] nanos) {
        Arrays.sort(nanos);
        double p50 = nanos[nanos.length / 2] / 1000.0;
        double p99 = nanos[(int) (nanos.length * 0.99)] / 1000.0;
        return String.format("p50_us=%.1f p99_us=%.1f n=%d", p50, p99, nanos.length);
    }

    public static void main(String[] args) throws Exception {
        String backend = args[0];
        boolean is64 = args[1].equals("arm64");
        String mode = args[2];

        if (mode.equals("latency")) {
            long t0 = System.nanoTime();
            Baseline b = new Baseline(backend, is64);
            long initNs = System.nanoTime() - t0;
            long t1 = System.nanoTime();
            byte[] first = b.aesCbc();
            long firstNs = System.nanoTime() - t1;
            System.out.printf("init_ms=%.1f first_call_us=%.1f%n", initNs / 1e6, firstNs / 1e3);

            for (int i = 0; i < 1000; i++) b.aesCbc();
            long[] lat = new long[10000];
            byte[] out = null;
            for (int i = 0; i < lat.length; i++) {
                long s = System.nanoTime();
                out = b.aesCbc();
                lat[i] = System.nanoTime() - s;
            }
            System.out.println("aesCbc16 " + stats(lat) + " out=" + sha(out) + " first_out=" + sha(first));

            for (int i = 0; i < 20; i++) b.pbkdf2(1000);
            long[] pl = new long[200];
            byte[] pout = null;
            for (int i = 0; i < pl.length; i++) {
                long s = System.nanoTime();
                pout = b.pbkdf2(1000);
                pl[i] = System.nanoTime() - s;
            }
            System.out.println("pbkdf2_1000 " + stats(pl) + " out=" + sha(pout));
            System.out.printf("guest_allocated_kb=%d guest_resident_kb=%d%n",
                    b.emulator.getBackend().getMemAllocatedSize() / 1024,
                    b.emulator.getBackend().getMemResidentSize() / 1024);
            b.emulator.close();
        } else {
            int instances = Integer.parseInt(args[3]);
            int seconds = Integer.parseInt(args[4]);
            List<Baseline> list = new ArrayList<>();
            long t0 = System.nanoTime();
            for (int i = 0; i < instances; i++) list.add(new Baseline(backend, is64));
            System.out.printf("init_all_ms=%.1f%n", (System.nanoTime() - t0) / 1e6);
            for (Baseline b : list) for (int i = 0; i < 500; i++) b.aesCbc();
            AtomicLong calls = new AtomicLong();
            long deadline = System.nanoTime() + seconds * 1_000_000_000L;
            List<Thread> threads = new ArrayList<>();
            for (Baseline b : list) {
                Thread t = new Thread(() -> {
                    long n = 0;
                    while (System.nanoTime() < deadline) { b.aesCbc(); n++; }
                    calls.addAndGet(n);
                });
                threads.add(t);
                t.start();
            }
            for (Thread t : threads) t.join();
            System.out.printf("instances=%d calls_per_s=%.0f%n", instances, calls.get() / (double) seconds);
            for (Baseline b : list) b.emulator.close();
        }
    }
}
