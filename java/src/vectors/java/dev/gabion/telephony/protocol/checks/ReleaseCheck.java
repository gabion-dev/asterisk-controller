// java/src/vectors/java/dev/gabion/telephony/protocol/checks/ReleaseCheck.java

package dev.gabion.telephony.protocol.checks;

import dev.gabion.telephony.protocol.ControllerRelease;

/**
 * The controller release the library carries, against the build that made it.
 *
 * The version must be the controller's — the one the build read from the controller's manifest —
 * and the checksums must be there exactly when the build was given the release's archives: a
 * published library without them would leave an application unable to tell the controller it
 * installs, and one built from a checkout must not pretend to know them. Every checksum must be a
 * whole SHA-256 and the address of every archive must name the version. The build runs it as part
 * of {@code check}.
 *
 * Usage: java ReleaseCheck <version> published|unpublished
 */
public final class ReleaseCheck {

    private ReleaseCheck() {
    }

    /**
     * Run the check.
     *
     * @param arguments the version the build gave the library, and whether it was published
     */
    public static void main(String[] arguments) {
        if (arguments.length != 2) {
            fail("usage: ReleaseCheck <version> published|unpublished");
        }
        String expectedVersion = arguments[0];
        boolean expectedPublished = "published".equals(arguments[1]);
        ControllerRelease release = ControllerRelease.ofThisLibrary();
        if (!release.version().equals(expectedVersion)) {
            fail("the library carries controller " + release.version() + ", the build is " + expectedVersion);
        }
        if (release.published() != expectedPublished) {
            fail("the library " + (release.published() ? "carries" : "lacks")
                    + " archive checksums; the build was " + arguments[1]);
        }
        if (expectedPublished && release.archives().size() != 4) {
            fail("the library carries " + release.archives().size() + " archive checksums, not 4");
        }
        for (var archive : release.archives().entrySet()) {
            String address = release.archiveUrl(archive.getKey()).toString();
            if (!address.equals("https://github.com/gabion-dev/asterisk-controller/releases/download/"
                    + expectedVersion + "/" + archive.getKey())) {
                fail(archive.getKey() + " is downloaded from " + address);
            }
        }
        System.out.println("The library carries controller " + release.version() + ", "
                + (release.published() ? "with the checksums of its " + release.archives().size() + " archives"
                        : "without archive checksums (not published)"));
    }

    private static void fail(String problem) {
        System.err.println("ReleaseCheck: " + problem);
        System.exit(1);
    }
}
