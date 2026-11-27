import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.StringObject;
import com.github.unidbg.linux.android.dvm.VM;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Runs Native.signals (libjnifix.so): raise() to a SA_SIGINFO handler, a SIGSEGV handler that
 * leaves faulting reads with siglongjmp, and a blocked signal delivered when unblocked.
 * Usage: SignalRun <arm64|arm32> <libjnifix.so> <output file>
 */
public class SignalRun {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[0].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(new DynarmicFactory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        vm.setJni(new AbstractJni() {});
        DalvikModule module = vm.loadLibrary(new File(args[1]), true);
        DvmClass nativeClass = vm.resolveClass("fix/Native");
        String result;
        try {
            StringObject out = nativeClass.callStaticJniMethodObject(emulator, "signals()Ljava/lang/String;");
            result = out == null ? "null result\n" : out.getValue();
        } catch (Throwable t) {
            result = "failed: " + t + "\n";
        }
        Files.write(Path.of(args[2]), result.getBytes(StandardCharsets.UTF_8));
        emulator.close();
        System.out.println(result);
    }
}
