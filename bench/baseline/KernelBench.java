import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.memory.Memory;
import com.github.unidbg.pointer.UnidbgPointer;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;

/**
 * The backend benchmark kernels (tools/backendbench/fixtures) called through unidbg's eFunc,
 * to compare unidbg's call path with the Rust backend layer on identical guest code.
 * Usage: KernelBench <unicorn2|dynarmic> <arm64|arm32> <fixtures dir>
 */
public class KernelBench {
    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        Path dir = Path.of(args[2]);
        String name = is64 ? "arm64" : "thumb";
        byte[] image = Files.readAllBytes(dir.resolve(name + ".bin"));
        Map<String, Long> sym = new HashMap<>();
        for (String line : Files.readAllLines(dir.resolve(name + ".sym"))) {
            String[] p = line.split(" ");
            sym.put(p[0], Long.parseLong(p[1], 16) - 0x10000);
        }
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(args[0].equals("dynarmic") ? new DynarmicFactory(false) : new Unicorn2Factory(false));
        long t0 = System.nanoTime();
        AndroidEmulator emulator = builder.build();
        Memory memory = emulator.getMemory();
        memory.setLibraryResolver(new AndroidResolver(23));
        UnidbgPointer code = memory.mmap(0x1000, 7);
        code.write(0, image, 0, image.length);
        UnidbgPointer data = memory.mmap(0x100000, 3);
        byte[] words = new byte[4096];
        for (int i = 0; i < 1024; i++) {
            int v = i * (int) 2654435761L;
            words[i * 4] = (byte) v; words[i * 4 + 1] = (byte) (v >> 8); words[i * 4 + 2] = (byte) (v >> 16); words[i * 4 + 3] = (byte) (v >> 24);
        }
        data.write(0, words, 0, words.length);
        long thumb = is64 ? 0 : 1;
        long empty = code.peer + sym.get("empty") | thumb;
        long checksum = code.peer + sym.get("checksum") | thumb;
        long copy = code.peer + sym.get("copy") | thumb;
        System.out.printf("init_us=%.1f%n", (System.nanoTime() - t0) / 1e3);
        // 32-bit guests take 32-bit arguments; a Java long would be split across a register pair.
        Number src = is64 ? (Number) data.peer : (Number) (int) data.peer;
        Number dst = is64 ? (Number) (data.peer + 0x8000) : (Number) (int) (data.peer + 0x8000);

        long cs = emulator.eFunc(checksum, src, 1024, 100).longValue() & 0xffffffffL;
        long cp = emulator.eFunc(copy, dst, src, 4096, 50).longValue() & 0xffffffffL;
        System.out.printf("checksum=%d copy=%d%n", cs, cp);

        for (int i = 0; i < 1000; i++) emulator.eFunc(empty);
        long[] lat = new long[100000];
        for (int i = 0; i < lat.length; i++) {
            long s = System.nanoTime();
            emulator.eFunc(empty);
            lat[i] = System.nanoTime() - s;
        }
        print("empty", lat);
        long[] c = new long[500];
        for (int i = 0; i < c.length; i++) {
            long s = System.nanoTime();
            emulator.eFunc(checksum, src, 1024, 100);
            c[i] = System.nanoTime() - s;
        }
        print("checksum_102400", c);
        for (int i = 0; i < c.length; i++) {
            long s = System.nanoTime();
            emulator.eFunc(copy, dst, src, 4096, 50);
            c[i] = System.nanoTime() - s;
        }
        print("copy_204800", c);
        emulator.close();
    }

    static void print(String label, long[] v) {
        Arrays.sort(v);
        System.out.printf("%s p50_us=%.3f p99_us=%.3f n=%d%n", label, v[v.length / 2] / 1e3, v[(int) (v.length * 0.99)] / 1e3, v.length);
    }
}
