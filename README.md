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

## Starting it

Whoever runs a node starts two processes, the controller first:

```sh
asterisk-controller --node <name> --listen 127.0.0.1:<port> \
    --application <ws://host:port/path> \
    --asterisk-tree <directory> --state <directory> \
    --asterisk-http 127.0.0.1:<port> --audio-ports <first>-<last> \
    [--sip <address:port>] [--sip-public <address>]
```

These are facts of the machine and of the launch: where the Asterisk tree and
the node's state directory are, and which ports are this node's. Before it
prints `asterisk-controller ready`, the controller has written Asterisk's
whole configuration into `<state>/etc`; Asterisk is then started on
`<state>/etc/asterisk.conf` and on nothing else. The configuration directory
is rewritten on every start: it is a derivative, never a place to edit.

The controller then finds Asterisk by itself and connects to its control
interface, and finds it again whenever the connection is lost. So either
process may be restarted next to the other. The secret the controller
presents to Asterisk is made once for a state directory and kept in it,
readable by the owner alone: a controller started beside a running Asterisk
must present the one that Asterisk read when it started.

**A node without its controller is for whoever started it to mend.** While the
controller is away, Asterisk turns new calls to the fallbacks of their entries
(below) and keeps the calls it has. It does not end a call that is in the
application with nobody to control it. If the controller cannot be started
again, Asterisk must be stopped too.

The controller drives only the Asterisk versions it knows. It reads the
version from the tree's own build record and refuses any other, before
Asterisk is started.

`--sip` is where Asterisk listens for telephone operators. It is needed only
when the settings name an operator; without operators Asterisk listens for
none. It cannot be changed while Asterisk runs — unlike everything the
settings say, which Asterisk takes in without dropping a call.

## The node's settings

What the node does with calls — its operators, outbound lines, entries with
their fallbacks, prompts — is its settings: the `Settings` of the protocol
description, given by the application. The node keeps the settings it last
applied in `<state>/settings.json` and starts on them, so a node restarted
while the application is away still knows what it serves. A node without the
file serves nothing and says so.

Settings are judged twice before anything is made of them: by the protocol
description, and then for what the description cannot say — that an operator,
line or prompt named in one place exists in another, and that every value can
be written into Asterisk's configuration and be read back as itself. Settings
that fail are refused whole, with the reason. The fingerprint of applied
settings is the SHA-256 of the file exactly as given.

Out of the settings the controller makes Asterisk's side of them:

- an operator becomes a trunk Asterisk recognises **only by the address its
  calls come from** — never by what a caller says about itself; a call from
  any other address is refused by Asterisk before it becomes a channel;
- a dialled-number entry becomes the dialplan rule that leads to the node's
  application — and says what becomes of a call that cannot enter it. A call
  cannot enter while the controller is not connected; it then tries again
  every second, five times, and after that Asterisk carries out the entry's
  fallback by itself: a transfer to the fallback's number on its line, showing
  the line's number, or a hang-up. The transfer's number is judged when the
  settings are checked, like any call the node places. It is the only thing
  in the dialplan that dials, and nothing a caller sends chooses it;
- the operator's identifier, which may be any text, never appears in
  Asterisk's configuration; the name used there is derived from it.

Not made yet: fetching settings from the application and applying a change
while running; prompts — until then a fallback that is a message is only a
hang-up; TLS towards an operator (settings carry no certificate
— such an operator is refused); operators that require registration.

## Outbound calls

The application adds a participant by dialling a number on a line. The
telephony module has checked the number before it sent the command; the node
checks it again by itself, because outbound calls are where telephone fraud
takes its money and this line of defence must hold even if the application's
own check was wrong:

- the country of the number is told by numbering data compiled into the
  controller (the `phonenumber` library — the same data Google's
  libphonenumber carries), also inside a shared country code: the United
  States, Canada and Jamaica all begin with `+1`;
- a number the data does not know as valid, and a premium-rate number, is
  never dialled, whatever a line allows;
- the country must be among the line's, and among those its operator carries;
- the line carries no more outbound calls at once than its limit — counted
  over the whole node, and a call's place is given back however it ends;
- the number shown to the called party is the line's. No command chooses it.

How the call ended is told truthfully: answered, busy, not answered in the
time the command allowed, or failed with the telephone network's own code.

## Layout

| Path                                 | Contents                                                     |
|--------------------------------------|--------------------------------------------------------------|
| `protocol/node-protocol.schema.json` | The protocol description: messages of the conversation and   |
|                                      | service connections and the settings the application gives   |
|                                      | the node — the single source of the types of both sides      |
| `protocol/messages.vectors.json`     | Messages every implementation must accept, or refuse, the    |
|                                      | same                                                         |
| `protocol/audio-frames.md`           | The binary audio frames of a conversation connection         |
| `protocol/audio-frames.vectors.json` | Frames every implementation must decode, or refuse, the same |
| `crates/asterisk-controller/`        | The controller: writes Asterisk's configuration, connects to |
|                                      | its control interface, accepts its media connections, opens  |
|                                      | a conversation connection to the application for each call,  |
|                                      | translates between the two                                   |
| `scripts/fetch-asterisk.sh`          | Fetches the Asterisk build the controller is pinned to       |
| `crates/node-protocol/`              | Rust: message types generated from the description, the      |
|                                      | checked decoding, the audio frames                           |
| `crates/protocol-java/`              | Generator of the Java message types                          |
| `java/`                              | The Java library of the application side, a Gradle project:  |
|                                      | the checked decoding and the audio frames written by hand,   |
|                                      | the message types generated by its build, and the program    |
|                                      | that runs the library through the vectors                    |
| `crates/repository-checks/`          | Checks of the repository itself, run by `cargo test`         |

## The protocol description is the judge

`protocol/node-protocol.schema.json` is the single source of the message types
of both sides: the Rust types here are generated from it at build time, and
the Java library of the application is generated from it by
`crates/protocol-java`. Neither side keeps a hand-written copy.

It is also the judge of what is accepted. On both sides a message is first
checked against the description itself and only then turned into its type;
the types alone are not trusted with that. Decoding derived from types lets
through things the description forbids — an extra field on a message that has
none, `null` for a field that may only be absent, a number written as text —
and which of them depends on the library and its settings. The shared vectors
found such holes on the Rust side before the checked decoding existed. In
Rust, decoding JSON directly is refused by the linter (`clippy.toml`); the
few places that must do it say so with a reason.

The description may use only the words both checkers enforce. The build stops
on any other word and names the place (`crates/node-protocol/build.rs`): a
type generator silently drops some of them — `minItems` and `maximum` were
found that way. A rule that cannot be stated in those words becomes a refusal
with a reason at the point where it is checked, never a line nobody enforces.

## Two sides, one set of vectors

`protocol/messages.vectors.json` and `protocol/audio-frames.vectors.json` list
messages and frames together with what must happen to each: accepted — and
then encoded back to the same thing — or refused. The Rust tests read them.
The build of the Java library (`java/`) generates its types from the
description, compiles everything with a real compiler, warnings as errors,
against the Jackson version the Gabion framework uses, and runs the library
through the same files. A message one side accepts and the other refuses
cannot go unnoticed.

The libraries the Java side is built against are checked against pinned
checksums (`java/gradle/verification-metadata.xml`).

An editor's null analysis of the Java library is configured the way the
Gabion framework configures its own: the annotation names are pinned in
`.vscode/settings.json`, and the few deviations from the analyzer's defaults
live in `java/config/jdt/null-analysis.prefs`, each with its reason. Warnings
about values that come from libraries without null annotations are off there
on purpose — no change of this code removes them.

## What Asterisk's behaviour imposes

**The controller connects to Asterisk, not Asterisk to the controller.**
Asterisk can open control connections itself, and the controller was built on
that twice before it was built this way.

- *A connection for each call* lives exactly as long as the channel it was
  opened for. Asterisk closes it a few seconds after that channel leaves,
  whoever else is still in the call, and the others are left with nobody in
  control.
- *One connection for the node, opened by Asterisk,* keeps the node's
  application in existence while the controller is away. A call that arrives
  then enters it and waits — with no end, if the controller does not return.
  And while Asterisk drops a connection it opened, an event it writes to it
  can crash it: Asterisk 22.11.0 died of a segmentation fault in its
  WebSocket write when its peer let go of the connection at the wrong moment.

To Asterisk an application exists while someone is connected for it. With the
controller as the one who connects, a controller that is away is a fact
Asterisk knows: a new call fails to enter at once and goes on through the
dialplan to its fallback, while the calls already there stay as they are —
people connected to each other go on talking. When the controller is back, it
hears of those calls again. What happened while it was away it is not told.

**Requests go to Asterisk over HTTP, one after another within a
conversation.** "Create a bridge" must reach Asterisk before "put a channel
into it". Requests of different conversations do not wait for each other.
The answer to a request and the event about the same channel come in no
particular order, and nothing here relies on one.

**What is in the node's application when the controller connects is
removed.** While nobody is connected nothing can enter the application, so
what is in it then is left from before: a controller that was restarted. This
build does not resume such calls; nobody is left on a line no one controls.

**A participant's audio path is a media channel joined to the participant's
channel.** Asterisk opens one more connection to the controller for the media
channel — to the path `/media`, through the connection its configuration names
`gabion-media`. Asterisk closes it; the controller ends the channel with a
request and reads the media connection to its end.
Asterisk can also tap a channel instead of joining it, but audio put into a
tap never reaches a participant who is connected to nobody else; both were
tried against the pinned build.

**Asterisk is handed a few seconds of audio at most.** Its media channel holds
twenty seconds and silently drops what does not fit. The controller keeps the
rest of a participant's queue itself and hands over more as Asterisk reports
what it has played.

**"Started", "delivered" and "dropped after so many milliseconds" are
Asterisk's words, not a clock's.** The controller puts marks between the
pieces of audio it hands over, and Asterisk reports each mark when it reaches
it. On a flush Asterisk is paused, asked how much it still holds, and only
then told to drop it.

## Checks

The Gabion development environment does not check this repository, so the
compiler is the one check that always runs, and it is strict for every crate:
unsafe code is forbidden, warnings are errors, the linter runs in its pedantic
mode, and code paths that could panic (`unwrap`, `expect`, indexing) do not
compile.

The controller is checked **against a real Asterisk** — the build it is pinned
to (`scripts/fetch-asterisk.sh`, release and checksums pinned there). The test
starts that Asterisk and the built controller, plays the application's side of
the conversation connection through the protocol library, and runs real calls:

- a call that rings, is answered and hung up by the caller;
- a conversation the handler ends;
- an application that declines, and one that breaks the protocol;
- an application that hears a caller and plays to them;
- a call that arrives from the operator over SIP, with audio through the trunk;
- a call from an address that is no operator's, which Asterisk must refuse;
- a ringing call turned away, with the reason reaching the caller's network;
- an application that dials: numbers it may not dial, a call that is
  answered — where the network must have asked the node for its credentials
  and seen the line's number — a line at its limit, calls that end busy,
  unanswered and failed, and a caller who hangs up before the one who was
  dialled;
- a node without its controller: the controller is killed in the middle of a
  call; calls that arrive then are given their fallbacks by Asterisk alone —
  one transferred through the operator, one hung up on; a call still waiting
  when a controller is started again is served, and the call from before is
  removed.

Asterisk runs on the configuration the controller wrote out of the test's
settings, and must load it without a single warning; what Asterisk then says
it read of the operator's credentials must be what the settings gave, special
characters included. The telephone network is a second Asterisk configured by
the test. Its stranger calls from `127.0.0.2`, so that must be an address of
the machine (it is on Linux).

In the first scenario with audio the caller's telephone is a media channel of the same Asterisk, speaking
the telephone network's audio format: the test measures the caller's tone in
what the application receives and the application's tone in what the caller
receives, flushes a queue in the middle of a segment, and has the caller hang
up with a segment still queued. After the scenarios that end badly the test
also requires that nobody is left on the line, and up to the killing of the
controller — that the controller and Asterisk never lost each other. Without an Asterisk tree the test fails and says how to
get one — it never skips itself. When it fails, it says whether Asterisk and
the controller are still alive and shows everything each of them printed.

Every file states its own path from the repository root at its top, with an
empty line under it, so that a file seen on its own says where it lives. A test holds the repository to it;
the file types a comment would break (JSON) and the files that must not carry
one (`README.md`, `LICENSE`, files written by cargo and Gradle) are listed in
that test by name,
and a file of a type the test does not know fails it.

```sh
cargo fmt --all --check
cargo clippy --all-targets
scripts/fetch-asterisk.sh      # once: the Asterisk the controller is pinned to
cargo test
(cd java && ./gradlew check)   # needs a JDK 21
```

The same four run on every push (`.github/workflows/checks.yml`).

## Status

Early development. Done: the protocol (description, Rust types, Java library,
audio frames, shared vectors) and the first slice of the controller — a call
from the telephone network carried from its first ring to its end, with the
commands answer, reject, remove, send digits and end, and with audio: the
application hears a participant, queues segments for them and flushes the
queue. The controller writes Asterisk's configuration out of the node's stored
settings, and adds participants by dialling. Not done yet: connecting
participants to each other, recording, holding a participant whose connection
is lost, standing in for an absent application, settings fetched from the
application, browser calls. A command the controller does not carry
out yet ends the conversation with an error that names it; none is accepted
and ignored. No release has been published.

## License

[Business Source License 1.1](LICENSE) — the license of the Gabion framework,
with the same parameters.

Asterisk is a registered trademark of Sangoma Technologies. This project is
NOT affiliated with, endorsed by, or sponsored by Sangoma Technologies or the
Asterisk project.
