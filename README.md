# asterisk-controller

The program that runs next to [Asterisk](https://www.asterisk.org/) on a
[Gabion](https://github.com/gabion-dev) telephony node and controls it on
behalf of the application.

## What it is for

A Gabion application handles phone calls through its telephony module. The
module does not speak Asterisk's language. It speaks the Gabion node
protocol — conversations, participants, commands and events — and this
controller translates that into Asterisk control and back. Everything that is
specific to Asterisk lives here: its control interface, its configuration
files, and the rules its behaviour imposes.

The controller also stands in for the application while the application is
away — during a deployment or a restart:

- people who are already talking to each other stay connected;
- a caller who was alone with the application gets the fallback the
  application configured for that entry;
- a participant left with nobody to talk to and nobody in charge is hung up.

It chooses no policy of its own: it carries out what the telephony module
gave it in advance.

## What reaches it from the network

Nothing. The controller listens on the loopback address only, for Asterisk on
the same machine. Every connection to the application is opened by the
controller. A telephony node accepts call audio from the network and has no
place that accepts commands.

## Layout

| Path                                | Contents                                                         |
|-------------------------------------|------------------------------------------------------------------|
| `protocol/conversation.schema.json` | The protocol description for a conversation connection — the     |
|                                     | single source of the message types of both sides                 |
| `crates/node-protocol/`             | Rust message types, generated from the description at build time |

## The protocol description is the single source

The Rust types here and the Java types of the application are both generated
from `protocol/`; neither side keeps a hand-written copy. The generated types
are strict: a field the description does not name, a missing required field,
or a message, command or event of an unknown kind is a decoding error —
nothing is skipped and nothing is defaulted.

The build refuses a description that promises more than the types check. The
type generator silently drops some JSON Schema keywords (`minItems` was found
that way), so `crates/node-protocol/build.rs` stops the build on any keyword
outside the list it knows to be enforced, and names the place.

## Checks

The Gabion development environment does not check this repository, so the
compiler is the one check that always runs, and it is strict for every crate:
unsafe code is forbidden, warnings are errors, the linter runs in its pedantic
mode, and code paths that could panic (`unwrap`, `expect`, indexing) do not
compile.

```sh
cargo fmt --all --check
cargo clippy --all-targets
cargo test
```

## Status

Early development: the protocol description of the conversation connection
and its Rust types. The controller itself is not written yet; no release has
been published.

## License

[Business Source License 1.1](LICENSE) — the license of the Gabion framework,
with the same parameters.

Asterisk is a registered trademark of Sangoma Technologies. This project is
NOT affiliated with, endorsed by, or sponsored by Sangoma Technologies or the
Asterisk project.
