import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Module;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.memory.Memory;
import com.github.unidbg.memory.MemoryBlock;
import com.github.unidbg.pointer.UnidbgPointer;

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Runs libparity.so's parity_run under unidbg and writes the text it produced, followed by the
 * return value, to a file: the expected output for the Rust parity test.
 * Usage: ParityRun <unicorn2|dynarmic> <arm64|arm32> <libparity.so> <output file>
 */
public class ParityRun {
    static final int SIZE = 16384;

    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(args[0].equals("dynarmic") ? new DynarmicFactory(false) : new Unicorn2Factory(false));
        builder.setProcessName("app");
        AndroidEmulator emulator = builder.build();
        Memory memory = emulator.getMemory();
        memory.setLibraryResolver(new AndroidResolver(23));
        Module module = emulator.loadLibrary(new File(args[2]), true);
        MemoryBlock block = memory.malloc(SIZE, true);
        UnidbgPointer buffer = block.getPointer();
        // 32-bit guests take 32-bit arguments; a Java long would be split across a register pair.
        Object address = is64 ? (Object) buffer.peer : (Object) (int) buffer.peer;
        int length = module.callFunction(emulator, "parity_run", address, SIZE).intValue();
        byte[] text = length > 0 ? buffer.getByteArray(0, length) : new byte[0];
        byte[] tail = ("return " + length + "\n").getBytes();
        byte[] all = new byte[text.length + tail.length];
        System.arraycopy(text, 0, all, 0, text.length);
        System.arraycopy(tail, 0, all, text.length, tail.length);
        Files.write(Path.of(args[3]), all);
        emulator.close();
        System.out.println("wrote " + args[3] + " (" + text.length + " bytes)");
    }
}
