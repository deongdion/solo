import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Module;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.unpack.ElfUnpacker;
import com.github.unidbg.memory.Memory;

import java.io.File;
import java.nio.file.Files;

/**
 * unidbg's ElfUnpacker on libpacked.so (packed.c): registered from a module listener before the
 * init functions run, as its documentation says; then unlock() is called. Prints how many bytes
 * of the dump differ from the unencrypted build after the load and after unlock().
 * Usage: UnpackRun <dynarmic|unicorn2> <arm64|arm32> <libpacked.so> <libpacked_plain.so> <out.so>
 */
public class UnpackRun {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(args[0].equals("dynarmic") ? new DynarmicFactory(false) : new Unicorn2Factory(false));
        AndroidEmulator emulator = builder.build();
        Memory memory = emulator.getMemory();
        memory.setLibraryResolver(new AndroidResolver(23));
        File packed = new File(args[2]);
        byte[] plain = Files.readAllBytes(new File(args[3]).toPath());
        File out = new File(args[4]);
        out.delete();
        memory.addModuleListener((emu, loaded) -> {
            if (loaded.name.equals("libpacked.so")) {
                try {
                    new ElfUnpacker(Files.readAllBytes(packed.toPath()), out).register(emulator, loaded);
                } catch (Exception e) {
                    throw new IllegalStateException(e);
                }
            }
        });
        String stage = "load";
        try {
            Module module = memory.load(packed, true);
            System.out.printf("%s %s after load: %s%n", args[0], args[1], compare(out, plain));
            stage = "unlock";
            Number result = module.callFunction(emulator, "unlock");
            System.out.printf("%s %s unlock() returned 0x%x; after unlock: %s%n", args[0], args[1], result.longValue(), compare(out, plain));
        } catch (Throwable e) {
            System.out.printf("%s %s %s failed: %s%n", args[0], args[1], stage, e);
        } finally {
            emulator.close();
        }
    }

    private static String compare(File out, byte[] plain) throws Exception {
        if (!out.exists()) return "no dump written";
        byte[] dump = Files.readAllBytes(out.toPath());
        int differ = 0;
        for (int i = 0; i < Math.min(dump.length, plain.length); i++) if (dump[i] != plain[i]) differ++;
        return String.format("dump differs from the unencrypted build in %d bytes", differ);
    }
}
