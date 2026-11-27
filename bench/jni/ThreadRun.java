import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.StringObject;
import com.github.unidbg.linux.android.dvm.VM;
import com.github.unidbg.linux.android.dvm.jni.ProxyClassFactory;
import com.github.unidbg.linux.android.dvm.jni.ProxyDvmObject;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Runs Native.threads(n, rounds, parts) (libjnifix.so): pthread workers, a condition-variable ping-pong
 * and a spinning thread that needs preemption (PLAN.md M3).
 * Usage: ThreadRun <abstract|proxy> <arm64|arm32> <libjnifix.so> <output file> <n> <rounds> <parts>
 */
public class ThreadRun {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(new DynarmicFactory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        // Without the dispatcher unidbg runs no second thread at all (its default).
        emulator.getSyscallHandler().setEnableThreadDispatcher(true);
        VM vm = emulator.createDalvikVM();
        if (args[0].equals("proxy")) {
            vm.setDvmClassFactory(new ProxyClassFactory());
        } else {
            vm.setJni(new AbstractJni() {});
        }
        DalvikModule module = vm.loadLibrary(new File(args[2]), true);
        DvmClass nativeClass = vm.resolveClass("fix/Native");
        String result;
        try {
            StringObject out = nativeClass.callStaticJniMethodObject(emulator, "threads(III)Ljava/lang/String;", Integer.parseInt(args[4]), Integer.parseInt(args[5]), Integer.parseInt(args[6]));
            result = out == null ? "null result\n" : out.getValue();
        } catch (Throwable t) {
            result = "failed: " + t + "\n";
        }
        Files.write(Path.of(args[3]), result.getBytes(StandardCharsets.UTF_8));
        emulator.close();
        System.out.println(result);
    }
}
