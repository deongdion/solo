import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.arm.backend.DynarmicFactory;
import com.github.unidbg.arm.backend.Unicorn2Factory;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.AbstractJni;
import com.github.unidbg.linux.android.dvm.BaseVM;
import com.github.unidbg.linux.android.dvm.DalvikModule;
import com.github.unidbg.linux.android.dvm.DvmClass;
import com.github.unidbg.linux.android.dvm.DvmObject;
import com.github.unidbg.linux.android.dvm.StringObject;
import com.github.unidbg.linux.android.dvm.VM;
import com.github.unidbg.linux.android.dvm.VaList;
import com.github.unidbg.linux.android.dvm.VarArg;
import com.github.unidbg.linux.android.dvm.array.ByteArray;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Runs libjnifix.so under unidbg with the Java side of the fixture implemented as an AbstractJni,
 * and writes what the natives returned: the expected output for runtime/tests/jni.rs.
 * Usage: JniRun <unicorn2|dynarmic> <arm64|arm32> <libjnifix.so> <output file>
 */
public class JniRun extends AbstractJni {
    static final class HostState {
        int count;

        HostState(int count) {
            this.count = count;
        }
    }

    /** The formula fix/Host.mix applies; runtime/tests/jni.rs computes the same. */
    static int mix(boolean z, byte b, char c, short s, int i, long j, float f, double d) {
        return (z ? 1 : 0) + b * 3 + c * 5 + s * 7 + i * 11 + (int) (j % 1000003) + (int) (f * 4) + (int) (d * 8);
    }

    private final VM vm;

    JniRun(VM vm) {
        this.vm = vm;
    }

    @Override
    public long callStaticLongMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg varArg) {
        if (signature.equals("fix/Host->add(IJ)J")) return varArg.getIntArg(0) + varArg.getLongArg(1);
        return super.callStaticLongMethod(vm, dvmClass, signature, varArg);
    }

    // unidbg dispatches the va_list variants (CallXxxMethodV) to separate methods; without these
    // overrides the V calls fail with UnsupportedOperationException (PLAN.md F2).
    @Override
    public long callStaticLongMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        return callStaticLongMethod(vm, dvmClass, signature, vaList);
    }

    @Override
    public int callIntMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        return callIntMethod(vm, dvmObject, signature, vaList);
    }

    @Override
    public double callStaticDoubleMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg varArg) {
        // unidbg sends calls on registered natives to the handler, so the native's formula is
        // repeated here; the Rust runtime calls the registered native itself.
        if (signature.equals("fix/Native->sum(IJFD)D")) {
            return varArg.getIntArg(0) * 1000.0 + varArg.getLongArg(1) * 100.0 + varArg.getFloatArg(2) * 10.0 + varArg.getDoubleArg(3);
        }
        return super.callStaticDoubleMethod(vm, dvmClass, signature, varArg);
    }

    @Override
    public int callIntMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg varArg) {
        if (signature.equals("fix/Host->mix(ZBCSIJFD)I")) {
            return mix(varArg.getIntArg(0) != 0, (byte) varArg.getIntArg(1), (char) varArg.getIntArg(2), (short) varArg.getIntArg(3),
                    varArg.getIntArg(4), varArg.getLongArg(5), varArg.getFloatArg(6), varArg.getDoubleArg(7));
        }
        return super.callIntMethod(vm, dvmObject, signature, varArg);
    }

    @Override
    public double callDoubleMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg varArg) {
        if (signature.equals("fix/Host->scale(FD)D")) return varArg.getFloatArg(0) * varArg.getDoubleArg(1);
        return super.callDoubleMethod(vm, dvmObject, signature, varArg);
    }

    @Override
    public DvmObject<?> callObjectMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg varArg) {
        switch (signature) {
            case "fix/Host->echo(Ljava/lang/String;)Ljava/lang/String;": {
                StringObject s = varArg.getObjectArg(0);
                return new StringObject(vm, s.getValue() + "!");
            }
            case "fix/Host->reverse([B)[B": {
                ByteArray a = varArg.getObjectArg(0);
                byte[] in = a.getValue();
                byte[] out = new byte[in.length];
                for (int i = 0; i < in.length; i++) out[i] = in[in.length - 1 - i];
                return new ByteArray(vm, out);
            }
        }
        return super.callObjectMethod(vm, dvmObject, signature, varArg);
    }

    @Override
    public void callVoidMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg varArg) {
        if (signature.equals("fix/Host->boom()V")) {
            vm.throwException(vm.resolveClass("java/lang/IllegalStateException").newObject("boom"));
            return;
        }
        super.callVoidMethod(vm, dvmObject, signature, varArg);
    }

    @Override
    public DvmObject<?> newObject(BaseVM vm, DvmClass dvmClass, String signature, VarArg varArg) {
        if (signature.equals("fix/Host-><init>(I)V")) return dvmClass.newObject(new HostState(varArg.getIntArg(0)));
        return super.newObject(vm, dvmClass, signature, varArg);
    }

    @Override
    public int getIntField(BaseVM vm, DvmObject<?> dvmObject, String signature) {
        if (signature.equals("fix/Host->count:I")) return ((HostState) dvmObject.getValue()).count;
        return super.getIntField(vm, dvmObject, signature);
    }

    @Override
    public void setIntField(BaseVM vm, DvmObject<?> dvmObject, String signature, int value) {
        if (signature.equals("fix/Host->count:I")) {
            ((HostState) dvmObject.getValue()).count = value;
            return;
        }
        super.setIntField(vm, dvmObject, signature, value);
    }

    @Override
    public DvmObject<?> getStaticObjectField(BaseVM vm, DvmClass dvmClass, String signature) {
        if (signature.equals("fix/Host->NAME:Ljava/lang/String;")) return new StringObject(vm, "fixture");
        return super.getStaticObjectField(vm, dvmClass, signature);
    }

    public static void main(String[] args) throws Exception {
        boolean is64 = args[1].equals("arm64");
        AndroidEmulatorBuilder builder = is64 ? AndroidEmulatorBuilder.for64Bit() : AndroidEmulatorBuilder.for32Bit();
        builder.addBackendFactory(args[0].equals("dynarmic") ? new DynarmicFactory(false) : new Unicorn2Factory(false));
        builder.setProcessName("app");
        AndroidEmulator emulator = builder.build();
        emulator.getMemory().setLibraryResolver(new AndroidResolver(23));
        VM vm = emulator.createDalvikVM();
        vm.setJni(new JniRun(vm));
        DalvikModule module = vm.loadLibrary(new File(args[2]), true);
        module.callJNI_OnLoad(emulator);
        DvmClass nativeClass = vm.resolveClass("fix/Native");
        DvmObject<?> host = vm.resolveClass("fix/Host").newObject(new HostState(7));
        StringBuilder out = new StringBuilder();
        StringObject run = nativeClass.callStaticJniMethodObject(emulator, "run(Lfix/Host;)Ljava/lang/String;", host);
        out.append(run.getValue());
        StringObject hello = nativeClass.callStaticJniMethodObject(emulator, "hello(Ljava/lang/String;)Ljava/lang/String;",
                new StringObject(vm, "wörld"));
        out.append("static ").append(hello.getValue()).append('\n');
        out.append("overload ").append(nativeClass.callStaticJniMethodInt(emulator, "twice(I)I", 21)).append('\n');
        out.append("count after ").append(((HostState) host.getValue()).count).append('\n');
        Files.write(Path.of(args[3]), out.toString().getBytes(StandardCharsets.UTF_8));
        emulator.close();
        System.out.println("wrote " + args[3]);
    }
}
