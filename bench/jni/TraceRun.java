import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Module;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.VM;

import java.io.File;
import java.io.OutputStream;
import java.io.PrintStream;

/**
 * Cost of unidbg's instruction trace (traceCode, unicorn2 backend; dynarmic has none) on
 * Native.hooked (libjnifix.so), output discarded, against the same calls untraced.
 * Usage: TraceRun <arm64|arm32> <libjnifix.so>
 */
public class TraceRun {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[0].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(new Unicorn2Factory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        vm.setJni(new AbstractJni() {});
        DalvikModule dm = vm.loadLibrary(new File(args[1]), true);
        Module module = dm.getModule();
        DvmClass c = vm.resolveClass("fix/Native");
        int n = 2000;
        if (args.length > 2 && args[2].equals("cold")) {
            // Trace before the module's code runs at all: lines for the first call.
            final long[] cold = {0};
            emulator.traceCode(module.base, module.base + module.size).setRedirect(new PrintStream(new OutputStream() {
                @Override
                public void write(int b) {
                    if (b == 10) cold[0]++;
                }
            }));
            c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
            System.out.printf("%s unidbg unicorn2: traceCode set before the first call: %d trace lines%n", args[0], cold[0]);
            emulator.close();
            return;
        }
        for (int i = 0; i < 20000; i++) c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        long started = System.nanoTime();
        for (int i = 0; i < n; i++) c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        double plain = (System.nanoTime() - started) / 1000.0 / n;
        final long[] lines = {0};
        PrintStream sink = new PrintStream(new OutputStream() {
            @Override
            public void write(int b) {
                if (b == 10) lines[0]++;
            }
        });
        emulator.traceCode(module.base, module.base + module.size).setRedirect(sink);
        int value = 0;
        started = System.nanoTime();
        for (int i = 0; i < n; i++) value = c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        double traced = (System.nanoTime() - started) / 1000.0 / n;
        emulator.close();
        System.out.printf("%s unidbg unicorn2: untraced %.2f us/call, traceCode on the module %.1f us/call (result %d, %.1f trace lines/call)%n", args[0], plain, traced, value, lines[0] / (double) n);
    }
}
