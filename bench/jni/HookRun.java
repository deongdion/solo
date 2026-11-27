import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Emulator;
import com.github.unidbg.Module;
import com.github.unidbg.arm.HookStatus;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.hook.HookContext;
import com.github.unidbg.hook.ReplaceCallback;
import com.github.unidbg.hook.hookzz.HookZz;
import com.github.unidbg.hook.xhook.IxHook;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.XHookImpl;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.VM;

import java.io.File;

/**
 * Cost of a hook in unidbg on Native.hooked (libjnifix.so): no hook, HookZz replace on fix_add
 * calling the original (with and without postCall), xHook on this module's strlen import.
 * Usage: HookRun <arm64|arm32> <libjnifix.so>
 */
public class HookRun {
    static int hooked(AndroidEmulator emulator, DvmClass c) {
        return c.callStaticJniMethodInt(emulator, "hooked(II)I", 2, 3);
    }

    static void time(AndroidEmulator emulator, DvmClass c, String label) {
        for (int i = 0; i < 1000; i++) hooked(emulator, c);
        int n = 20000;
        int value = 0;
        long started = System.nanoTime();
        for (int i = 0; i < n; i++) value = hooked(emulator, c);
        double us = (System.nanoTime() - started) / 1000.0 / n;
        System.out.printf("%s: %.2f us/call (result %d)%n", label, us, value);
    }

    public static void main(String[] args) throws Exception {
        String arch = args[0];
        String mode = args.length > 2 ? args[2] : "all";
        boolean is64 = arch.equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(new DynarmicFactory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        vm.setJni(new AbstractJni() {});
        DalvikModule dm = vm.loadLibrary(new File(args[1]), true);
        Module module = dm.getModule();
        DvmClass c = vm.resolveClass("fix/Native");
        if (mode.equals("none")) time(emulator, c, arch + " unidbg no hook");
        if (mode.equals("hookzz")) {
            HookZz.getInstance(emulator).replace(module.findSymbolByName("fix_add"), new ReplaceCallback() {
                @Override
                public HookStatus onCall(Emulator<?> emulator, HookContext context, long originFunction) {
                    return HookStatus.RET(emulator, originFunction);
                }
            });
            time(emulator, c, arch + " unidbg HookZz replace -> original (fix_add)");
        }
        if (mode.equals("hookzzpost")) {
            HookZz.getInstance(emulator).replace(module.findSymbolByName("fix_add"), new ReplaceCallback() {
                @Override
                public HookStatus onCall(Emulator<?> emulator, HookContext context, long originFunction) {
                    return HookStatus.RET(emulator, originFunction);
                }
                @Override
                public void postCall(Emulator<?> emulator, HookContext context) {
                }
            }, true);
            time(emulator, c, arch + " unidbg HookZz replace + postCall (fix_add)");
        }
        if (mode.equals("xhook")) {
            IxHook xHook = XHookImpl.getInstance(emulator);
            xHook.register("libjnifix.so", "strlen", new ReplaceCallback() {
                @Override
                public HookStatus onCall(Emulator<?> emulator, HookContext context, long originFunction) {
                    return HookStatus.RET(emulator, originFunction);
                }
            });
            xHook.refresh();
            time(emulator, c, arch + " unidbg xHook strlen -> original");
        }
        emulator.close();
    }
}
