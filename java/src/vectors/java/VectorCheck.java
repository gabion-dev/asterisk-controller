// java/src/vectors/java/VectorCheck.java

import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import tools.jackson.databind.JsonNode;
import tools.jackson.databind.json.JsonMapper;

/**
 * The Java protocol library against the shared vectors.
 *
 * The same two files — messages and audio frames — are read by the Rust tests of the controller.
 * A message one side accepts and the other refuses, or a frame they decode differently, would be
 * a protocol the two only think they share. This check runs the library — its generated types
 * and its hand-written part, compiled by a real compiler — through every vector. The build runs
 * it as part of {@code check}.
 *
 * The library's package is given on the command line, so the check does not depend on the
 * package the application chooses.
 *
 * Usage: java VectorCheck <package> <messages.vectors.json> <audio-frames.vectors.json>
 */
public final class VectorCheck {

    private static final JsonMapper MAPPER = JsonMapper.builder().build();

    private VectorCheck() {}

    public static void main(String[] arguments) throws Exception {
        if (arguments.length != 3) {
            System.err.println(
                    "usage: java VectorCheck <package> <messages.vectors.json> <audio-frames.vectors.json>");
            System.exit(2);
        }
        String protocolPackage = arguments[0];
        List<String> problems = new ArrayList<>();
        int messages = checkMessages(protocolPackage, Path.of(arguments[1]), problems);
        int frames = checkFrames(protocolPackage, Path.of(arguments[2]), problems);

        if (!problems.isEmpty()) {
            problems.forEach(System.err::println);
            System.err.println(problems.size() + " problem(s)");
            System.exit(1);
        }
        System.out.println("Java: " + messages + " message vectors and " + frames
                + " audio frame vectors hold");
    }

    private static int checkMessages(String protocolPackage, Path file, List<String> problems)
            throws Exception {
        Class<?> json = Class.forName(protocolPackage + ".ProtocolJson");
        Method decode = json.getMethod("decode", JsonNode.class, Class.class);
        Method encode = json.getMethod("encode", Object.class);
        Class<?> violation = Class.forName(protocolPackage + ".ProtocolViolation");

        JsonNode vectors = MAPPER.readTree(Files.readString(file)).get("vectors");
        if (vectors.size() < 60) {
            problems.add("the message vector file lost its vectors");
        }
        for (JsonNode vector : vectors) {
            String name = vector.get("name").stringValue();
            boolean accept = vector.get("accept").booleanValue();
            JsonNode value = vector.get("json");
            Class<?> type = Class.forName(protocolPackage + "." + vector.get("type").stringValue());
            Object decoded;
            try {
                decoded = decode.invoke(null, value, type);
            } catch (java.lang.reflect.InvocationTargetException e) {
                if (!violation.isInstance(e.getCause())) {
                    // A fault of the library itself must fail the check, not count as a refusal.
                    problems.add(name + ": the library failed: " + e.getCause());
                } else if (accept) {
                    problems.add(name + ": refused but must be accepted (" + e.getCause().getMessage() + ")");
                }
                continue;
            }
            if (!accept) {
                problems.add(name + ": accepted but must be refused");
                continue;
            }
            JsonNode encoded = MAPPER.readTree((String) encode.invoke(null, decoded));
            if (!encoded.equals(value)) {
                problems.add(name + ": accepted, but encodes back as " + encoded + " instead of " + value);
            }
        }
        return vectors.size();
    }

    private static int checkFrames(String protocolPackage, Path file, List<String> problems)
            throws Exception {
        Class<?> audioFrame = Class.forName(protocolPackage + ".AudioFrame");
        Method decode = audioFrame.getMethod("decode", byte[].class);
        Method encode = audioFrame.getMethod("encode", audioFrame);
        Class<?> refused = Class.forName(protocolPackage + ".AudioFrame$Refused");
        Frames frames = new Frames(decode, encode, refused, HexFormat.of());

        JsonNode vectors = MAPPER.readTree(Files.readString(file)).get("vectors");
        if (vectors.size() < 20) {
            problems.add("the audio frame vector file lost its vectors");
        }
        for (JsonNode vector : vectors) {
            String name = vector.get("name").stringValue();
            byte[] frame = frames.hex().parseHex(vector.get("frame_hex").stringValue());
            JsonNode expect = vector.get("expect");
            JsonNode refusedAs = vector.get("refused");
            if (expect != null && refusedAs == null) {
                expectAccepted(name, frame, expect, frames, problems);
            } else if (expect == null && refusedAs != null) {
                expectRefused(name, frame, refusedAs.stringValue(), frames, problems);
            } else {
                problems.add(name + ": a vector states exactly one of expect and refused");
            }
        }
        return vectors.size();
    }

    /** The library's audio frame operations, found by name in the package under check. */
    private record Frames(Method decode, Method encode, Class<?> refused, HexFormat hex) {}

    /** The frame must decode to what the vector expects and encode back to the same bytes. */
    private static void expectAccepted(String name, byte[] frame, JsonNode expect, Frames frames,
            List<String> problems) throws Exception {
        Object decoded;
        try {
            decoded = frames.decode().invoke(null, (Object) frame);
        } catch (java.lang.reflect.InvocationTargetException e) {
            problems.add(frames.refused().isInstance(e.getCause())
                    ? name + ": refused but must be accepted"
                    : name + ": the library failed: " + e.getCause());
            return;
        }
        compare(name, expect, decoded, frames.hex(), problems);
        byte[] encoded = (byte[]) frames.encode().invoke(null, decoded);
        if (!java.util.Arrays.equals(encoded, frame)) {
            problems.add(name + ": does not encode back to itself");
        }
    }

    /** The frame must be refused, and for the reason the vector names. */
    private static void expectRefused(String name, byte[] frame, String refusedAs, Frames frames,
            List<String> problems) throws Exception {
        try {
            frames.decode().invoke(null, (Object) frame);
        } catch (java.lang.reflect.InvocationTargetException e) {
            if (!frames.refused().isInstance(e.getCause())) {
                // A fault of the library itself must fail the check, not count as a refusal.
                problems.add(name + ": the library failed: " + e.getCause());
                return;
            }
            String reason = frames.refused().getMethod("refusal").invoke(e.getCause()).toString()
                    .toLowerCase(java.util.Locale.ROOT);
            if (!reason.equals(refusedAs)) {
                problems.add(name + ": refused as " + reason + " instead of " + refusedAs);
            }
            return;
        }
        problems.add(name + ": accepted but must be refused as " + refusedAs);
    }

    /** Compare a decoded frame with what the vector expects, field by field. */
    private static void compare(String name, JsonNode expect, Object decoded, HexFormat hex,
            List<String> problems) throws Exception {
        String kind = expect.get("kind").stringValue();
        if (!decoded.getClass().getSimpleName().equalsIgnoreCase(kind)) {
            problems.add(name + ": decoded as " + decoded.getClass().getSimpleName()
                    + " instead of " + kind);
            return;
        }
        expectEqual(name, "participant", expect.get("participant").stringValue(),
                valueOf(field(decoded, "participant")), problems);
        expectEqual(name, "audio", expect.get("audio_hex").stringValue(),
                hex.formatHex((byte[]) field(decoded, "audio")), problems);
        if (kind.equals("heard")) {
            // The position is unsigned on the wire; Java carries it in a signed long.
            expectEqual(name, "position", expect.get("position_ms").bigIntegerValue().toString(),
                    Long.toUnsignedString((long) field(decoded, "positionMs")), problems);
        } else {
            expectEqual(name, "segment", expect.get("segment").stringValue(),
                    valueOf(field(decoded, "segment")), problems);
            expectEqual(name, "last", String.valueOf(expect.get("last").booleanValue()),
                    String.valueOf(field(decoded, "last")), problems);
        }
    }

    private static Object field(Object record, String name) throws Exception {
        return record.getClass().getMethod(name).invoke(record);
    }

    private static String valueOf(Object identifier) throws Exception {
        return (String) field(identifier, "value");
    }

    private static void expectEqual(String name, String what, String expected, String actual,
            List<String> problems) {
        if (!expected.equals(actual)) {
            problems.add(name + ": " + what + " is " + actual + " instead of " + expected);
        }
    }
}
