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
import java.util.HashMap;
import java.util.Map;

/**
 * Passes a java.util.HashMap to Native.walk (libjnifix.so), which reads it through Map, Set and
 * Iterator, the way natives read request parameters (PLAN.md F11).
 * Usage: MapRun <abstract|proxy> <arm64|arm32> <libjnifix.so> <output file>
 * abstract: unidbg's AbstractJni with the map wrapped by ProxyDvmObject.createObject;
 * proxy: ProxyClassFactory, which calls the real Java methods by reflection.
 */
public class MapRun {
    static final String[] WORDS = {"zeta", "alpha", "beta", "gamma", "delta", "epsilon", "eta", "theta", "iota", "kappa"};

    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(new DynarmicFactory(false));
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        if (args[0].equals("proxy")) {
            vm.setDvmClassFactory(new ProxyClassFactory());
        } else {
            vm.setJni(new AbstractJni() {});
        }
        DalvikModule module = vm.loadLibrary(new File(args[2]), true);
        DvmClass nativeClass = vm.resolveClass("fix/Native");
        Map<String, Integer> map = new HashMap<>();
        for (int i = 0; i < WORDS.length; i++) map.put(WORDS[i], i);
        String result;
        try {
            StringObject out = nativeClass.callStaticJniMethodObject(emulator, "walk(Ljava/util/Map;)Ljava/lang/String;",
                    ProxyDvmObject.createObject(vm, map));
            result = out == null ? "null result\n" : out.getValue();
        } catch (Throwable t) {
            result = "failed: " + t + "\n";
        }
        Files.write(Path.of(args[3]), result.getBytes(StandardCharsets.UTF_8));
        emulator.close();
        System.out.println(result);
    }
}
