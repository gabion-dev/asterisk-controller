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

| Path                                 | Contents                                                     |
|--------------------------------------|--------------------------------------------------------------|
| `protocol/node-protocol.schema.json` | The protocol description: messages of the conversation and   |
|                                      | service connections and the settings the application gives   |
|                                      | the node — the single source of the types of both sides      |
| `protocol/audio-frames.md`           | The binary audio frames of a conversation connection         |
| `protocol/audio-frames.vectors.json` | Frames every implementation must decode, or refuse, the same |
| `crates/node-protocol/`              | Rust message types generated from the description, and the   |
|                                      | audio frame implementation                                   |
| `crates/repository-checks/`          | Checks of the repository itself, run by `cargo test`         |

## The protocol description is the single source

The Rust types here and the Java types of the application are both generated
from `protocol/node-protocol.schema.json`; neither side keeps a hand-written
copy. The generated types are strict: a field the description does not name,
a missing required field, or a message, command, event or request of an
unknown kind is a decoding error — nothing is skipped and nothing is
defaulted.

The build refuses a description that promises more than the types check. The
type generator silently drops some JSON Schema keywords (`minItems` and
`maximum` were found that way), so `crates/node-protocol/build.rs` stops the
build on any keyword outside the list it knows to be enforced, and names the
place. A rule the types cannot carry becomes a refusal with a reason at the
point where it is checked, never a line of the description nobody enforces.

Audio frames are binary and written by hand on each side. What keeps the two
implementations together is the vector file: both must pass it.

## Checks

The Gabion development environment does not check this repository, so the
compiler is the one check that always runs, and it is strict for every crate:
unsafe code is forbidden, warnings are errors, the linter runs in its pedantic
mode, and code paths that could panic (`unwrap`, `expect`, indexing) do not
compile.

Every file states its own path from the repository root at its top, so that a
file seen on its own says where it lives. A test holds the repository to it;
the file types a comment would break (JSON) and the files that must not carry
one (`README.md`, `LICENSE`, `Cargo.lock`) are listed in that test by name,
and a file of a type the test does not know fails it.

```sh
cargo fmt --all --check
cargo clippy --all-targets
cargo test
```

## Status

Early development: the protocol — its description, the Rust types and the
audio frames. The controller itself is not written yet; no release has been
published.

## License

[Business Source License 1.1](LICENSE) — the license of the Gabion framework,
with the same parameters.

Asterisk is a registered trademark of Sangoma Technologies. This project is
NOT affiliated with, endorsed by, or sponsored by Sangoma Technologies or the
Asterisk project.
