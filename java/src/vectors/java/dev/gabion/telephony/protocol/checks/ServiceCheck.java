// java/src/vectors/java/dev/gabion/telephony/protocol/checks/ServiceCheck.java

package dev.gabion.telephony.protocol.checks;

import dev.gabion.telephony.protocol.ApplicationServiceMessage;
import dev.gabion.telephony.protocol.NodeMessage;
import dev.gabion.telephony.protocol.ProtocolJson;
import dev.gabion.telephony.protocol.Settings;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.TimeUnit;

/**
 * The Java protocol library in a real exchange with the controller.
 *
 * The shared vectors hold both sides to the same messages; they do not show the two talking. This
 * program plays the application on the controller's service connection with nothing but the
 * library: it starts the controller built from this repository — without Asterisk, which the
 * service connection does not need — takes its connection, reads its hello, welcomes it, gives it
 * settings twice and pings it, decoding and encoding every message with {@code ProtocolJson}. The
 * settings fingerprints the controller answers with are compared with the library's own.
 *
 * The WebSocket server is written here, a few dozen lines of RFC 6455: the JDK has a WebSocket
 * client and no server, and the check takes no library the application would not.
 *
 * Usage: java ServiceCheck <controller binary>
 */
public final class ServiceCheck {

    private static final String NODE = "java-check";
    private static final String SECRET = "the secret of the java check";
    private static final int STEP_MS = 15_000;

    private ServiceCheck() {}

    public static void main(String[] arguments) throws Exception {
        if (arguments.length != 1) {
            System.err.println("usage: java ServiceCheck <controller binary>");
            System.exit(2);
        }
        Path root = Files.createTempDirectory("service-check");
        Path tree = Files.createDirectories(root.resolve("tree"));
        Files.writeString(tree.resolve("BUILD-INFO.txt"),
                "asterisk-version: 22.11.0\nmodules:\n  res_ari.so\n");
        Path state = Files.createDirectories(root.resolve("node"));
        Path secret = Files.writeString(state.resolve("node-secret"), SECRET);
        Files.setPosixFilePermissions(secret, PosixFilePermissions.fromString("rw-------"));

        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            server.setSoTimeout(STEP_MS);
            Process controller = new ProcessBuilder(arguments[0],
                    "--node", NODE,
                    "--listen", "127.0.0.1:" + freePort(),
                    "--application", "ws://127.0.0.1:" + server.getLocalPort(),
                    "--asterisk-tree", tree.toString(),
                    "--state", state.toString(),
                    "--asterisk-http", "127.0.0.1:" + freePort(),
                    "--audio-ports", "40000-40001")
                    .redirectErrorStream(true)
                    .redirectOutput(root.resolve("controller.log").toFile())
                    .start();
            try (Socket socket = server.accept()) {
                socket.setSoTimeout(STEP_MS);
                exchange(new Connection(socket));
            } catch (IOException | ProtocolFailure e) {
                System.err.println("the controller's output:");
                System.err.println(Files.readString(root.resolve("controller.log")));
                throw e;
            } finally {
                controller.destroy();
                controller.waitFor(STEP_MS, TimeUnit.MILLISECONDS);
            }
        }
        System.out.println("Java: the controller's service connection holds");
    }

    /** The service connection, from the node's hello to a pong. */
    private static void exchange(Connection connection) throws Exception {
        connection.acceptUpgrade("/service", "Basic " + Base64.getEncoder()
                .encodeToString((NODE + ":" + SECRET).getBytes(StandardCharsets.UTF_8)));

        NodeMessage hello = ProtocolJson.decode(connection.readText(), NodeMessage.class);
        if (!(hello instanceof NodeMessage.Hello(var running, var asterisk, var controllerVersion,
                var node, var protocol))
                || running != null || !NODE.equals(node) || protocol != 1
                || controllerVersion.isEmpty() || !"22.11.0".equals(asterisk)) {
            throw new ProtocolFailure("the node said " + hello + " instead of its hello");
        }
        send(connection, new ApplicationServiceMessage.Welcome());

        // Settings without anything in them change none of Asterisk's files: they are applied
        // without Asterisk, and the fingerprint the node gives back is the library's own.
        Settings nothing = new Settings(List.of(), List.of(), List.of(), List.of());
        send(connection, new ApplicationServiceMessage.Settings(nothing));
        NodeMessage applied = ProtocolJson.decode(connection.readText(), NodeMessage.class);
        String nothingFingerprint = ProtocolJson.fingerprint(nothing);
        if (!(applied instanceof NodeMessage.SettingsApplied(var appliedFingerprint))
                || !appliedFingerprint.value().equals(nothingFingerprint)) {
            throw new ProtocolFailure("settings without anything in them were answered " + applied
                    + "; their fingerprint is " + nothingFingerprint);
        }

        // Settings whose line names an operator they do not have are refused, with the reason
        // and the fingerprint of what was given.
        Settings disagreeing = ProtocolJson.decode("""
                {"operators": [], "prompts": [],
                 "lines": [{"id": "main", "number": "+19715870050", "operator": "nobody",
                            "allowed_countries": ["US"], "concurrent_outbound_limit": 1}],
                 "entries": []}
                """, Settings.class);
        send(connection, new ApplicationServiceMessage.Settings(disagreeing));
        NodeMessage refused = ProtocolJson.decode(connection.readText(), NodeMessage.class);
        String disagreeingFingerprint = ProtocolJson.fingerprint(disagreeing);
        if (!(refused instanceof NodeMessage.SettingsRefused(var refusedFingerprint, var problem))
                || !refusedFingerprint.value().equals(disagreeingFingerprint) || problem.isEmpty()) {
            throw new ProtocolFailure("settings whose parts disagree were answered " + refused
                    + "; their fingerprint is " + disagreeingFingerprint);
        }

        send(connection, new ApplicationServiceMessage.Ping(7));
        NodeMessage pong = ProtocolJson.decode(connection.readText(), NodeMessage.class);
        if (!(pong instanceof NodeMessage.Pong(var n)) || n != 7) {
            throw new ProtocolFailure("a ping was answered " + pong);
        }
    }

    private static void send(Connection connection, ApplicationServiceMessage message)
            throws Exception {
        connection.writeText(ProtocolJson.encode(message, ApplicationServiceMessage.class));
    }

    private static int freePort() throws IOException {
        try (ServerSocket socket = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            return socket.getLocalPort();
        }
    }

    /** The exchange went otherwise than the protocol says. */
    private static final class ProtocolFailure extends Exception {
        private static final long serialVersionUID = 1L;

        ProtocolFailure(String message) {
            super(message);
        }
    }

    /** The server's end of one WebSocket connection (RFC 6455), text frames only. */
    private static final class Connection {

        private static final String ACCEPT_SUFFIX = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

        private final InputStream in;
        private final OutputStream out;

        Connection(Socket socket) throws IOException {
            this.in = socket.getInputStream();
            this.out = socket.getOutputStream();
        }

        /** Read the opening request, require its path and credentials, and agree to it. */
        void acceptUpgrade(String path, String authorization) throws Exception {
            List<String> lines = new ArrayList<>();
            ByteArrayOutputStream line = new ByteArrayOutputStream();
            while (true) {
                int b = in.read();
                if (b < 0) {
                    throw new ProtocolFailure("the controller closed the connection while opening it");
                }
                if (b == '\n') {
                    String text = line.toString(StandardCharsets.US_ASCII).strip();
                    line.reset();
                    if (text.isEmpty()) {
                        break;
                    }
                    lines.add(text);
                } else {
                    line.write(b);
                }
            }
            if (lines.isEmpty() || !lines.get(0).startsWith("GET " + path + " ")) {
                throw new ProtocolFailure("the controller asked for " + lines);
            }
            String key = header(lines, "sec-websocket-key");
            if (!authorization.equals(header(lines, "authorization"))) {
                throw new ProtocolFailure("the controller did not present the node's secret");
            }
            String accept = Base64.getEncoder().encodeToString(MessageDigest.getInstance("SHA-1")
                    .digest((key + ACCEPT_SUFFIX).getBytes(StandardCharsets.US_ASCII)));
            out.write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                    + "Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n")
                    .getBytes(StandardCharsets.US_ASCII));
            out.flush();
        }

        private static String header(List<String> lines, String name) throws ProtocolFailure {
            for (String line : lines.subList(1, lines.size())) {
                int colon = line.indexOf(':');
                if (colon > 0 && line.substring(0, colon).toLowerCase(Locale.ROOT).equals(name)) {
                    return line.substring(colon + 1).strip();
                }
            }
            throw new ProtocolFailure("the controller's request has no " + name);
        }

        /** The next text message; WebSocket pings are answered on the way. */
        String readText() throws Exception {
            while (true) {
                int first = readByte();
                int second = readByte();
                int opcode = first & 0x0F;
                long length = second & 0x7F;
                if (length == 126) {
                    length = ((long) readByte() << 8) | readByte();
                } else if (length == 127) {
                    length = 0;
                    for (int i = 0; i < 8; i++) {
                        length = (length << 8) | readByte();
                    }
                }
                byte[] mask = (second & 0x80) != 0 ? in.readNBytes(4) : new byte[4];
                byte[] payload = in.readNBytes(Math.toIntExact(length));
                for (int i = 0; i < payload.length; i++) {
                    payload[i] ^= mask[i % 4];
                }
                if ((first & 0x80) == 0) {
                    throw new ProtocolFailure("the controller sent a fragmented message");
                }
                switch (opcode) {
                    case 0x1 -> {
                        return new String(payload, StandardCharsets.UTF_8);
                    }
                    case 0x9 -> writeFrame(0xA, payload);
                    case 0xA -> { }
                    case 0x8 -> throw new ProtocolFailure("the controller closed the connection: "
                            + new String(payload, StandardCharsets.UTF_8));
                    default -> throw new ProtocolFailure("the controller sent frame " + opcode);
                }
            }
        }

        void writeText(String text) throws IOException {
            writeFrame(0x1, text.getBytes(StandardCharsets.UTF_8));
        }

        private void writeFrame(int opcode, byte[] payload) throws IOException {
            ByteArrayOutputStream frame = new ByteArrayOutputStream();
            frame.write(0x80 | opcode);
            if (payload.length < 126) {
                frame.write(payload.length);
            } else if (payload.length < 65_536) {
                frame.write(126);
                frame.write(payload.length >>> 8);
                frame.write(payload.length & 0xFF);
            } else {
                frame.write(127);
                for (int shift = 56; shift >= 0; shift -= 8) {
                    frame.write((int) (((long) payload.length >>> shift) & 0xFF));
                }
            }
            frame.write(payload);
            out.write(frame.toByteArray());
            out.flush();
        }

        private int readByte() throws IOException, ProtocolFailure {
            int b = in.read();
            if (b < 0) {
                throw new ProtocolFailure("the controller closed the connection");
            }
            return b;
        }
    }
}
