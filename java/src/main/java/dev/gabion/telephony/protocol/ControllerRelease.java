// java/src/main/java/dev/gabion/telephony/protocol/ControllerRelease.java

package dev.gabion.telephony.protocol;

import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.net.URI;
import java.util.Map;
import java.util.Optional;
import java.util.Properties;
import java.util.TreeMap;

/**
 * The controller release this library was published with: its version, and the SHA-256 of each of
 * its four archives.
 *
 * <p>The library and the controller of one version speak one protocol, so an application that
 * takes the library knows which controller its telephony node must run, and how to tell an archive
 * is that one: the checksums were taken from the very archives the release built, and travel inside
 * the library. "Downloaded from the right address" proves nothing about what was downloaded.</p>
 *
 * <p>Only a published library carries the checksums; a library built from a checkout carries the
 * version alone, and {@link #published()} says so.</p>
 */
public final class ControllerRelease {

    /** The resource the build writes, next to this class. */
    private static final String RESOURCE = "controller-release.properties";

    /** Where the controller's releases are published. */
    private static final String DOWNLOADS =
            "https://github.com/gabion-dev/asterisk-controller/releases/download/";

    /** The archive file names of a release, one per platform the controller is built for. */
    private static final String[] ARCHIVES = {
        "asterisk-controller-linux-amd64.tar.gz",
        "asterisk-controller-linux-arm64.tar.gz",
        "asterisk-controller-darwin-arm64.tar.gz",
        "asterisk-controller-darwin-amd64.tar.gz",
    };

    private final String version;
    private final Map<String, String> archives;

    private ControllerRelease(String version, Map<String, String> archives) {
        this.version = version;
        this.archives = Map.copyOf(archives);
    }

    /**
     * The release of this library.
     *
     * @return the controller release the library was built with
     * @throws IllegalStateException the library carries no release record, or a broken one — a
     *     library not built by its own build
     */
    public static ControllerRelease ofThisLibrary() {
        Properties record = new Properties();
        try (InputStream in = ControllerRelease.class.getResourceAsStream(RESOURCE)) {
            if (in == null) {
                throw new IllegalStateException(
                        "the library carries no " + RESOURCE + ": it was not built by its own build");
            }
            record.load(in);
        } catch (IOException e) {
            throw new UncheckedIOException(RESOURCE + " cannot be read", e);
        }
        String version = record.getProperty("version");
        if (version == null || version.isBlank()) {
            throw new IllegalStateException(RESOURCE + " names no version");
        }
        Map<String, String> archives = new TreeMap<>();
        for (String archive : ARCHIVES) {
            String checksum = record.getProperty(archive);
            if (checksum != null) {
                if (!checksum.matches("[0-9a-f]{64}")) {
                    throw new IllegalStateException(
                            RESOURCE + " gives " + archive + " no whole SHA-256: " + checksum);
                }
                archives.put(archive, checksum);
            }
        }
        if (!archives.isEmpty() && archives.size() != ARCHIVES.length) {
            throw new IllegalStateException(
                    RESOURCE + " gives checksums of " + archives.size() + " archives, not of all "
                            + ARCHIVES.length);
        }
        return new ControllerRelease(version, archives);
    }

    /**
     * The controller's version — the one it names to the application in its hello.
     *
     * @return the version, such as {@code 0.1.0-dev.9}
     */
    public String version() {
        return version;
    }

    /**
     * Whether this is a published library, carrying the checksums of its release's archives.
     *
     * @return true when every archive's checksum is known
     */
    public boolean published() {
        return !archives.isEmpty();
    }

    /**
     * The SHA-256 of every archive of the release, by file name.
     *
     * @return lowercase hex checksums; empty for a library that was not published
     */
    public Map<String, String> archives() {
        return archives;
    }

    /**
     * The SHA-256 of one archive of the release.
     *
     * @param archive the archive's file name, such as {@code asterisk-controller-linux-amd64.tar.gz}
     * @return its lowercase hex checksum; empty for an archive the release does not have, or for a
     *     library that was not published
     */
    public Optional<String> sha256(String archive) {
        return Optional.ofNullable(archives.get(archive));
    }

    /**
     * Where an archive of the release is downloaded from.
     *
     * @param archive the archive's file name
     * @return its address
     */
    public URI archiveUrl(String archive) {
        return URI.create(DOWNLOADS + version + "/" + archive);
    }
}
