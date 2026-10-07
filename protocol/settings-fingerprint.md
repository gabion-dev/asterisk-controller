<!-- protocol/settings-fingerprint.md -->

# The fingerprint of settings

The node tells the application which settings it runs on by their
fingerprint: in its hello, and when it applies or refuses settings it was
sent. The application compares it with the fingerprint of the settings it
expects the node to run on. A difference is a node that does not do what the
application believes it does, and the application shows it.

So the fingerprint must depend on what the settings say and on nothing else —
not on which side wrote them, which library encoded them or in what order it
put their fields. It is the SHA-256 of their **canonical text**, written in
lowercase hexadecimal.

## The canonical text

The settings as JSON, written so that the same settings always give the same
bytes. These are the rules of RFC 8785 (JSON Canonicalization Scheme) for
values without fractional numbers — the protocol has none:

- no whitespace outside strings;
- the members of every object ordered by name, names compared as sequences of
  UTF-16 code units;
- array items in their own order;
- in a string, `"` is written `\"` and `\` is written `\\`; the control
  characters U+0008, U+0009, U+000A, U+000C and U+000D are written `\b`, `\t`,
  `\n`, `\f` and `\r`; every other character from U+0000 to U+001F is written
  `\u00` and two lowercase hexadecimal digits; every other character is
  written as itself;
- an integer in decimal, without a sign, a fraction, an exponent or leading
  zeros;
- the whole encoded as UTF-8.

`settings-fingerprint.vectors.json` holds values with their canonical text and
its SHA-256, which every implementation must reproduce. Some of its values
are not settings: they exercise the rules — escapes, characters outside the
Basic Multilingual Plane, names whose UTF-16 order differs from their UTF-8
order — which settings may meet in identifiers and passwords.
