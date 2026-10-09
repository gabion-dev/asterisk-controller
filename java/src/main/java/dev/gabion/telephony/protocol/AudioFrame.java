// java/src/main/java/dev/gabion/telephony/protocol/AudioFrame.java

package dev.gabion.telephony.protocol;

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Objects;

import org.jspecify.annotations.Nullable;

/**
 * One binary frame of a conversation connection.
 *
 * <p>The format is described in {@code audio-frames.md} of the controller's repository. This is
 * one of its two implementations — the other is the controller's — and the shared vectors are
 * what keeps the two together: both must decode, and refuse, the same frames.</p>
 */
public sealed interface AudioFrame {

    /** Bytes of twenty milliseconds of audio: 320 samples of 16 bits. */
    int HEARD_AUDIO_BYTES = 640;

    /** Most audio bytes one playback frame may carry. */
    int MAX_PLAYBACK_AUDIO_BYTES = 64_000;

    /**
     * What a participant says. Controller to application.
     *
     * @param participant who is speaking
     * @param positionMs  milliseconds of this participant's audio before the frame; unsigned
     * @param audio       exactly twenty milliseconds of audio
     */
    // The audio is bytes, as the frame carries it; a list of boxed bytes, fifty
    // times a second for each participant, would be no improvement. What the
    // check guards against is handled here: the array is copied in and out,
    // and equality, hashing and the text compare and show its bytes.
    @SuppressWarnings("ArrayRecordComponent")
    record Heard(ParticipantId participant, long positionMs, byte[] audio) implements AudioFrame {
        public Heard {
            Objects.requireNonNull(participant, "participant");
            audio = audio.clone();
        }

        @Override
        public byte[] audio() {
            return audio.clone();
        }

        /** Frames are equal when they say the same: the audio is compared by its bytes. */
        @Override
        public boolean equals(@Nullable Object other) {
            return other instanceof Heard heard
                    && participant.equals(heard.participant)
                    && positionMs == heard.positionMs
                    && Arrays.equals(audio, heard.audio);
        }

        @Override
        public int hashCode() {
            return Objects.hash(participant, positionMs, Arrays.hashCode(audio));
        }

        @Override
        public String toString() {
            return "Heard[participant=" + participant + ", positionMs=" + positionMs
                    + ", audio=" + audio.length + " bytes]";
        }
    }

    /**
     * Audio of a queued segment. Application to controller.
     *
     * @param participant who is to hear it
     * @param segment     the segment the play command named
     * @param last        whether this frame ends the segment
     * @param audio       a whole number of samples; may be empty only when last
     */
    // The audio is bytes, as the frame carries it; a list of boxed bytes, fifty
    // times a second for each participant, would be no improvement. What the
    // check guards against is handled here: the array is copied in and out,
    // and equality, hashing and the text compare and show its bytes.
    @SuppressWarnings("ArrayRecordComponent")
    record Playback(ParticipantId participant, SegmentId segment, boolean last, byte[] audio)
            implements AudioFrame {
        public Playback {
            Objects.requireNonNull(participant, "participant");
            Objects.requireNonNull(segment, "segment");
            audio = audio.clone();
        }

        @Override
        public byte[] audio() {
            return audio.clone();
        }

        /** Frames are equal when they say the same: the audio is compared by its bytes. */
        @Override
        public boolean equals(@Nullable Object other) {
            return other instanceof Playback playback
                    && participant.equals(playback.participant)
                    && segment.equals(playback.segment)
                    && last == playback.last
                    && Arrays.equals(audio, playback.audio);
        }

        @Override
        public int hashCode() {
            return Objects.hash(participant, segment, last, Arrays.hashCode(audio));
        }

        @Override
        public String toString() {
            return "Playback[participant=" + participant + ", segment=" + segment + ", last="
                    + last + ", audio=" + audio.length + " bytes]";
        }
    }

    /** Why bytes are not an audio frame. The names are those of the shared vectors. */
    enum Refusal {
        UNKNOWN_KIND, TRUNCATED, BAD_IDENTIFIER, WRONG_HEARD_LENGTH, WRONG_PLAYBACK_LENGTH,
        BAD_LAST_MARKER
    }

    /** Bytes are not an audio frame. */
    final class Refused extends Exception {

        private static final long serialVersionUID = 1L;

        private final Refusal refusal;

        Refused(Refusal refusal) {
            super("not an audio frame: " + refusal);
            this.refusal = refusal;
        }

        /**
         * Why the bytes were refused.
         *
         * @return the reason
         */
        public Refusal refusal() {
            return refusal;
        }
    }

    /**
     * Decode one binary frame.
     *
     * @param bytes the frame
     * @return the frame
     * @throws Refused when the format description refuses the bytes; no frame is repaired or
     *                 partly accepted
     */
    static AudioFrame decode(byte[] bytes) throws Refused {
        ByteBuffer frame = ByteBuffer.wrap(bytes);
        if (!frame.hasRemaining()) {
            throw new Refused(Refusal.TRUNCATED);
        }
        int kind = frame.get() & 0xFF;
        return switch (kind) {
            case 0x01 -> {
                String participant = takeIdentifier(frame);
                if (frame.remaining() < Long.BYTES) {
                    throw new Refused(Refusal.TRUNCATED);
                }
                long position = frame.getLong();
                if (frame.remaining() != HEARD_AUDIO_BYTES) {
                    throw new Refused(Refusal.WRONG_HEARD_LENGTH);
                }
                yield new Heard(new ParticipantId(participant), position, rest(frame));
            }
            case 0x02 -> {
                String participant = takeIdentifier(frame);
                String segment = takeIdentifier(frame);
                if (!frame.hasRemaining()) {
                    throw new Refused(Refusal.TRUNCATED);
                }
                int marker = frame.get() & 0xFF;
                if (marker > 1) {
                    throw new Refused(Refusal.BAD_LAST_MARKER);
                }
                boolean last = marker == 1;
                if (!playbackLengthIsValid(frame.remaining(), last)) {
                    throw new Refused(Refusal.WRONG_PLAYBACK_LENGTH);
                }
                yield new Playback(
                        new ParticipantId(participant), new SegmentId(segment), last, rest(frame));
            }
            default -> throw new Refused(Refusal.UNKNOWN_KIND);
        };
    }

    /**
     * Encode a frame.
     *
     * @param frame the frame
     * @return its bytes
     * @throws Refused when the frame's audio or identifiers are ones the format does not allow:
     *                 what cannot be decoded is never sent
     */
    static byte[] encode(AudioFrame frame) throws Refused {
        return switch (frame) {
            case Heard heard -> {
                byte[] audio = heard.audio();
                if (audio.length != HEARD_AUDIO_BYTES) {
                    throw new Refused(Refusal.WRONG_HEARD_LENGTH);
                }
                byte[] participant = identifier(heard.participant().value());
                yield ByteBuffer.allocate(1 + 1 + participant.length + Long.BYTES + audio.length)
                        .put((byte) 0x01)
                        .put((byte) participant.length).put(participant)
                        .putLong(heard.positionMs())
                        .put(audio)
                        .array();
            }
            case Playback playback -> {
                byte[] audio = playback.audio();
                if (!playbackLengthIsValid(audio.length, playback.last())) {
                    throw new Refused(Refusal.WRONG_PLAYBACK_LENGTH);
                }
                byte[] participant = identifier(playback.participant().value());
                byte[] segment = identifier(playback.segment().value());
                yield ByteBuffer
                        .allocate(1 + 1 + participant.length + 1 + segment.length + 1 + audio.length)
                        .put((byte) 0x02)
                        .put((byte) participant.length).put(participant)
                        .put((byte) segment.length).put(segment)
                        .put((byte) (playback.last() ? 1 : 0))
                        .put(audio)
                        .array();
            }
        };
    }

    private static boolean playbackLengthIsValid(int length, boolean last) {
        return length % 2 == 0 && length <= MAX_PLAYBACK_AUDIO_BYTES && (length > 0 || last);
    }

    private static String takeIdentifier(ByteBuffer frame) throws Refused {
        if (!frame.hasRemaining()) {
            throw new Refused(Refusal.TRUNCATED);
        }
        int length = frame.get() & 0xFF;
        if (length == 0 || length > 128) {
            throw new Refused(Refusal.BAD_IDENTIFIER);
        }
        if (frame.remaining() < length) {
            throw new Refused(Refusal.TRUNCATED);
        }
        byte[] bytes = new byte[length];
        frame.get(bytes);
        if (!printableAscii(bytes)) {
            throw new Refused(Refusal.BAD_IDENTIFIER);
        }
        return new String(bytes, StandardCharsets.US_ASCII);
    }

    private static byte[] identifier(String identifier) throws Refused {
        byte[] bytes = identifier.getBytes(StandardCharsets.US_ASCII);
        boolean sameText = new String(bytes, StandardCharsets.US_ASCII).equals(identifier);
        if (bytes.length == 0 || bytes.length > 128 || !sameText || !printableAscii(bytes)) {
            throw new Refused(Refusal.BAD_IDENTIFIER);
        }
        return bytes;
    }

    /** Whether every byte is a printable ASCII character, {@code !} to {@code ~}: the identifiers of the protocol. */
    private static boolean printableAscii(byte[] bytes) {
        for (byte b : bytes) {
            if (b < '!' || b > '~') {
                return false;
            }
        }
        return true;
    }

    private static byte[] rest(ByteBuffer frame) {
        return Arrays.copyOfRange(frame.array(), frame.arrayOffset() + frame.position(),
                frame.arrayOffset() + frame.limit());
    }
}
