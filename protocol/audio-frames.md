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
(1 to 128) followed by that many bytes of UTF-8.

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

## What is refused

A frame is refused — a protocol error, never a skipped frame — when:

- its first byte is neither `0x01` nor `0x02`;
- an identifier is empty, longer than 128 bytes, or not UTF-8;
- it ends before its content does;
- a heard frame carries anything but exactly 640 bytes of audio;
- a playback frame carries an odd number of audio bytes, more than 64 000, or
  none without being the last;
- the "last" byte is anything but `0x00` or `0x01`.
