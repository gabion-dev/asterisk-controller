<!-- protocol/audio-frames.md -->

# Audio frames

Audio between the application and a participant travels on the conversation
connection as **binary** WebSocket frames, next to the text messages described
in `node-protocol.schema.json`. This file is the description of those frames;
`audio-frames.vectors.json` holds frames that both sides must decode — or
refuse — identically.

## The audio itself

One format in both directions: signed 16-bit linear PCM, little-endian, mono,
16 kHz. Twenty milliseconds are 320 samples, 640 bytes.

## Frames

All integers are big-endian. An identifier is its length in one byte
(1 to 128) followed by that many bytes of printable ASCII (`!` to `~`) — the
identifiers of `node-protocol.schema.json` are printable ASCII, so each takes
one byte a character.

### Heard — controller to application

What a participant says, after the application asked to `listen`.

| Bytes | Content                                                             |
|-------|---------------------------------------------------------------------|
| 1     | `0x01`                                                              |
| 1 + n | participant identifier                                              |
| 8     | position: milliseconds of this participant's audio before the frame |
| 640   | exactly twenty milliseconds of audio                                |

Frames of one participant arrive in order, each twenty milliseconds after the
previous one. Silence is sent as audio like anything else: deciding what is
speech is not the node's business.

The position counts the participant's audio from the moment the node began to
receive it, whether the application was listening or not: after
`stop_listening` and a later `listen` the positions go on from where the
participant's audio is, so a pause in listening shows as a gap.

### Playback — application to controller

Audio of a segment the application queued with the `play` command.

| Bytes | Content                                                            |
|-------|--------------------------------------------------------------------|
| 1     | `0x02`                                                             |
| 1 + n | participant identifier                                             |
| 1 + m | segment identifier                                                 |
| 1     | `0x00` — more audio of this segment follows; `0x01` — the last one |
| any   | audio: a whole number of samples, at most 64 000 bytes             |

A segment is as long as the application makes it; the node paces it. A frame
that is not the last one carries at least one sample; the last one may be
empty.

Segments of one participant are played in the order they were queued, and a
segment whose last frame has not arrived holds back the ones behind it. The
node hands audio to the call in pieces of twenty milliseconds: a segment whose
audio is not a whole number of them is followed by silence up to the next one.
Segments that must follow each other without a gap are therefore cut at
multiples of 640 bytes.

A well-formed playback frame for a segment that is not in the participant's
queue — never queued, already dropped or delivered, or of a participant who
has left — is left out, not refused. The application may have sent it before
it could read that the `play` command was rejected, the queue flushed or the
participant gone.

## What is refused

A frame is refused — a protocol error, never a skipped frame — when:

- its first byte is neither `0x01` nor `0x02`;
- an identifier is empty, longer than 128 bytes, or not printable ASCII;
- it ends before its content does;
- a heard frame carries anything but exactly 640 bytes of audio;
- a playback frame carries an odd number of audio bytes, more than 64 000, or
  none without being the last;
- the "last" byte is anything but `0x00` or `0x01`.
