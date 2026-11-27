import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Module;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.VM;

import java.io.File;

/**
 * Cost of a breakpoint whose callback continues at once (Debugger.addBreakPoint) on Native.hooked.
 * Usage: BreakRun <dynarmic|unicorn2> <arm64|arm32> <libjnifix.so>
 */
public class BreakRun {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(args[0].equals("dynarmic") ? new DynarmicFactory(false) : new Unicorn2Factory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        vm.setJni(new AbstractJni() {});
        DalvikModule dm = vm.loadLibrary(new File(args[2]), true);
        Module module = dm.getModule();
        DvmClass c = vm.resolveClass("fix/Native");
        int n = 20000;
        if (args.length > 3 && args[3].equals("cold")) {
            // Breakpoint set before the module's code ran: hits and cost.
            final long[] hits = {0};
            long add = module.findSymbolByName("fix_add", false).getAddress();
            emulator.attach().addBreakPoint(add, (emu, address) -> {
                hits[0]++;
                return true;
            });
            int value = 0;
            for (int i = 0; i < 1000; i++) value = c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
            long started = System.nanoTime();
            for (int i = 0; i < n; i++) value = c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
            double hit = (System.nanoTime() - started) / 1000.0 / n;
            emulator.close();
            System.out.printf("%s %s unidbg cold: breakpoint hit and continued %.2f us/call (result %d, %d hits for %d calls)%n",
                    args[0], args[1], hit, value, hits[0], n + 1000);
            return;
        }
        for (int i = 0; i < 20000; i++) c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        long started = System.nanoTime();
        for (int i = 0; i < n; i++) c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        double plain = (System.nanoTime() - started) / 1000.0 / n;
        final long[] hits = {0};
        long add = module.findSymbolByName("fix_add", false).getAddress();
        emulator.attach().addBreakPoint(add, (emu, address) -> {
            hits[0]++;
            return true;
        });
        int value = 0;
        for (int i = 0; i < 1000; i++) c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        started = System.nanoTime();
        for (int i = 0; i < n; i++) value = c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
        double hit = (System.nanoTime() - started) / 1000.0 / n;
        emulator.close();
        System.out.printf("%s %s unidbg: no breakpoint %.2f us/call, breakpoint hit and continued %.2f us/call (result %d, %d hits)%n",
                args[0], args[1], plain, hit, value, hits[0]);
    }
}
