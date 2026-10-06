// java/ProtocolViolation.java

package __PACKAGE__;

/**
 * A message is not one the protocol description allows.
 *
 * <p>Checked on purpose: a refused message is an outcome every reader of the connection has to
 * decide about, not an accident to be caught somewhere above.</p>
 */
public final class ProtocolViolation extends Exception {

    private static final long serialVersionUID = 1L;

    private final String at;

    /**
     * @param at      where in the message, as a JSON pointer; empty for the message itself
     * @param problem what is wrong there
     */
    public ProtocolViolation(String at, String problem) {
        super((at.isEmpty() ? "the message" : at) + ": " + problem);
        this.at = at;
    }

    /**
     * Where in the message the problem is.
     *
     * @return a JSON pointer; empty for the message itself
     */
    public String at() {
        return at;
    }
}
