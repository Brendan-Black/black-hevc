import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.file.Path;

/**
 * Calls black-hevc from Java through the FFM API, as BrendanTv will:
 * {@code java --enable-native-access=ALL-UNNAMED Still.java <black_hevc.dll> <video.mp4> <seconds> <out.png> [max]}
 */
public class Still {
	public static void main(String[] args) throws Throwable {
		if (args.length < 4) {
			System.err.println("usage: Still <black_hevc library> <video.mp4> <seconds> <out.png> [max]");
			System.exit(2);
		}
		int max = args.length > 4 ? Integer.parseInt(args[4]) : 320;
		try (Arena arena = Arena.ofConfined()) {
			SymbolLookup lib = SymbolLookup.libraryLookup(Path.of(args[0]), arena);
			// int32_t black_hevc_still_file(const char *video, double seconds, uint32_t max,
			//                               const char *out, char *error, size_t error_cap)
			MethodHandle stillFile = Linker.nativeLinker().downcallHandle(
				lib.find("black_hevc_still_file").orElseThrow(),
				FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_DOUBLE,
					ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));
			MemorySegment error = arena.allocate(512);
			long start = System.nanoTime();
			int code = (int) stillFile.invokeExact(arena.allocateFrom(args[1]), Double.parseDouble(args[2]), max,
				arena.allocateFrom(args[3]), error, error.byteSize());
			long ms = (System.nanoTime() - start) / 1_000_000;
			if (code != 0) {
				System.err.println("failed (" + code + "): " + error.getString(0));
				System.exit(1);
			}
			System.out.println("wrote " + args[3] + " in " + ms + " ms");
		}
	}
}
