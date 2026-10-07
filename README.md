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

- people who are already talking to each other stay connected, and the
  application is looked for while they do; when an instance is there again
  it is handed the conversation as it is;
- a caller who was alone with the application gets the fallback the
  application configured for that entry — a transfer, which the controller
  carries out by calling the fallback's number and connecting the two, or a
  message, which until the node has prompts is a hang-up;
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
    --application <wss://host[:port][/path]> \
    --asterisk-tree <directory> --state <directory> \
    --asterisk-http 127.0.0.1:<port> --audio-ports <first>-<last> \
    [--sip <address:port>] [--sip-public <address>]
```

These are facts of the machine and of the launch: where the Asterisk tree and
the node's state directory are, which ports are this node's, and where the
application is. Before it
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

## The way to the application

Every connection between the node and the application is opened by the
controller, to the one address `--application` gives: one for each
conversation to `<address>/conversation`, and the node's one service
connection to `<address>/service`. Each of them proves the node by its name
and secret, as HTTP Basic authentication — the scheme every web stack checks
by itself, once per connection.

The secret is the node's alone. Whoever runs the node puts it into
`<state>/node-secret`, readable by its owner alone, and gives the application
the same one for the node's name; a controller without it, or with one others
may read, does not start. Across a network the secret travels only inside
TLS: an address other than the loopback one must be `wss://`, and the
controller refuses to start with any other. TLS is rustls with the ring
provider, compiled in — the same on every platform, with no TLS library of the
machine involved; the application's certificate is judged against the roots
the machine trusts. The node's name may hold letters, digits, `.`, `_` and
`-`: a colon would end it inside Basic authentication.

Both kinds of connection are kept alive the same way: each side sends a ping
at least every ten seconds, and thirty seconds with nothing from the other
side is a connection lost. Opening a connection is held to the same limit, so
an instance that takes a connection and never answers is as good as none.

## The service connection

On it the node says who it is — its name, its versions, and the fingerprint
of the settings it runs on — and is welcomed or refused. The application
gives the node its settings on it, whole, inside the message: when the
fingerprint in the hello is not the one it expects, and whenever the settings
change. A signal to fetch them elsewhere would make it possible for "the
settings changed" to arrive without the settings.

The node applies settings while calls go on. Of Asterisk's configuration only
two files follow from settings — the dialplan and the operators — and each is
read by one module. A file that changes is replaced whole, never half
written, and its module is reloaded; if Asterisk refuses, everything replaced
is put back and reloaded again, and the node answers that it refused the
settings, with the reason, and keeps the ones it had. Only settings Asterisk
took in are stored for the next start and used by conversations from then
on.

What the node does on its own, with no application to decide — keeps people
connected while it looks for an instance, carries out a fallback's transfer,
hangs up on someone or stops calling them — it reports. A report is written
to the journal in the state directory first (`<state>/reports.jsonl`) and
sent after, at once if the service connection is there and as soon as it is
otherwise; the application's word that it has the report is what removes it.
A controller that dies between doing something and telling it leaves the
report behind, and the next one sends it, under the same identifier.

The service connection is lost — it is opened again, a second later, to
whichever instance the address leads to. A request of the application that
this build cannot carry out ends the connection with the request's name.

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
settings is the SHA-256 of their canonical text
(`protocol/settings-fingerprint.md`): it depends on what the settings say,
not on how a file or a message happens to write them, so the application can
compare it with the fingerprint of the settings it expects.

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

Not made yet: prompts — until then a fallback that is a message is only a
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

## When the application is away

A conversation outlives its connection to the application. When the instance
that owns a conversation is lost — or none takes a conversation to begin
with — the controller looks once for another instance, through the same
address, and then does for the participants what their state calls for:

- people connected to each other are kept together, and an instance is
  looked for every second while they are; one that is there is handed the
  conversation as it is now — its participants, their states, who is
  connected to whom — and nothing of what the lost instance had asked for:
  no audio paths, no playback queues, no answers to its commands;
- a caller who was alone with the application is given the fallback of their
  entry, once: a transfer has the controller call the fallback's number on
  the fallback's line, showing the line's number and judging the number as
  it judges any, and connect the caller to whoever answers — a caller who
  still rings is answered at that moment, as a transfer answers them; the two
  are then people connected to each other, kept and handed over as above. A
  message is a hang-up until the node has prompts;
- whoever is left with nobody to talk to and nobody in charge is hung up:
  one being called by the lost instance, one it had added beside the
  caller, one of two connected by a fallback when the other leaves.

An instance that declines the conversation counts as none: a declining
instance is leaving, or has no handler for the entry, and the next look may
reach another one.

## Connecting participants

The application connects participants so that they hear each other, and takes
a participant out of the connection again. What the two commands mean:

- connecting needs two participants or more, each named once, and each of
  them in the conversation — one who still rings is not connected yet;
- whoever of them is connected to others already stays so: connecting a
  participant to one of a group connects them to the whole group, and
  connecting participants of two groups makes the groups one. Nothing but
  the command that takes a participant out separates anyone;
- one who is left alone in a group — the other was taken out, was removed or
  hung up — is connected to nobody;
- taking out a participant who is connected to nobody is accepted: they are
  already where the command wants them.

The application's own audio to and from a participant is not part of the
connection. It hears a connected participant by themselves, and what it plays
to one of them the others do not hear.

## Layout

| Path                                         | Contents                                                     |
|----------------------------------------------|--------------------------------------------------------------|
| `protocol/node-protocol.schema.json`         | The protocol description: messages of the conversation and   |
|                                              | service connections and the settings the application gives   |
|                                              | the node — the single source of the types of both sides      |
| `protocol/messages.vectors.json`             | Messages every implementation must accept, or refuse, the    |
|                                              | same                                                         |
| `protocol/audio-frames.md`                   | The binary audio frames of a conversation connection         |
| `protocol/audio-frames.vectors.json`         | Frames every implementation must decode, or refuse, the same |
| `protocol/settings-fingerprint.md`           | The canonical text of settings, whose SHA-256 is their       |
|                                              | fingerprint                                                  |
| `protocol/settings-fingerprint.vectors.json` | Values with the canonical text and digest every              |
|                                              | implementation must reproduce                                |
| `crates/asterisk-controller/`                | The controller: writes Asterisk's configuration, connects to |
|                                              | its control interface, accepts its media connections, opens  |
|                                              | a conversation connection to the application for each call,  |
|                                              | translates between the two                                   |
| `scripts/fetch-asterisk.sh`                  | Fetches the Asterisk build the controller is pinned to       |
| `crates/node-protocol/`                      | Rust: message types generated from the description, the      |
|                                              | checked decoding, the audio frames                           |
| `crates/protocol-java/`                      | Generator of the Java message types                          |
| `java/`                                      | The Java library of the application side, a Gradle project:  |
|                                              | the checked decoding and the audio frames written by hand,   |
|                                              | the message types generated by its build, and the program    |
|                                              | that runs the library through the vectors                    |
| `crates/repository-checks/`                  | Checks of the repository itself, run by `cargo test`         |

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
then encoded back to the same thing — or refused.
`protocol/settings-fingerprint.vectors.json` lists values with their canonical
text and its digest; the vectors were made by a third implementation, so
neither side can agree with itself by mistake. The Rust tests read them.
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

**What is in the node's application when the controller connects is from
before: a controller that was restarted.** While nobody is connected nothing
can enter the application, so what is in it then is exactly what the
controller before left. Its conversations carry on: what only a controller
knows of a participant — which conversation they are in, who they are in it,
where it came from, their number, the line they were called on — the
controller writes on the participant's channel in Asterisk as variables,
when a call arrives and, for one the node places, in the request that
places it (a channel the node is calling cannot be written on until it
answers). The next controller reads them back, with the channel's state and
the bridges it is in, and offers the conversation to the application as it
is, as it offers any it has lost the owner of. Media channels died with the
controller — Asterisk ends one when its connection goes, and does not open
it again — and the taps and bridges of audio paths are of no use to anyone:
they are removed, and audio paths are built again when asked for. Tried
against the pinned build: thirty-three media connections cut under audio did
not harm Asterisk.

**A participant's audio path is a media channel joined to the participant's
channel.** Asterisk opens one more connection to the controller for the media
channel — to the path `/media`, through the connection its configuration names
`gabion-media`. Asterisk closes it; the controller ends the channel with a
request and reads the media connection to its end.
Asterisk can also tap a channel instead of joining it, but audio put into a
tap never reaches a participant who is connected to nobody else; both were
tried against the pinned build.

**A participant connected to others is tapped; their audio path stays where it
is.** Participants the application connects to each other are put into a
bridge of their own, a group. The media channel does not follow a participant
there: everyone in the group would hear what the application says to one of
them, and the application would hear them all at once. A tap on the
participant's channel takes their place beside the media channel instead —
Asterisk's snoop channel, hearing what comes in from the participant and
whispering out to them. Tried against the pinned build: what is put into the
tap reaches a participant who is in a bridge with others at full level, also
when the others send no audio at all; nobody else hears it; what comes out of
the tap is the participant alone. The media channel, its connection and its
marks are untouched by the move there and back, so the application sees no
change.

**A name the controller gives in Asterisk is never given again.** Asterisk
keeps a bridge's name taken for as long as anything still holds the bridge —
also after the bridge was removed — and refuses a new bridge of that name.
So every name the controller gives — of a media channel, a tap, a bridge, a
group, a channel it dials — carries the controller's run, as does the
identifier of every participant it adds: a controller started again beside
a running Asterisk never meets a name the one before it gave, and a
participant is never given an identifier someone before them had in the
same conversation.

**A participant changes bridges with a gap, not an overlap.** Moving them
takes two requests, and between the two the media channel is either alone for
a moment or has two sources at once. Measured over twenty moves on the pinned
build, the first order costs about one frame of twenty milliseconds in each
direction, and the second doubles the audio for as long. The controller takes
the participant out first and puts the tap in second, and the other way round
on the way back.

**A refusal from Asterisk about a participant decides nothing by itself.** A
participant can hang up while the controller is arranging them. Asterisk then
refuses the request, and its refusal may arrive before its word that the
participant left. So the controller asks Asterisk about the channel: one that
is gone, or is no longer in the node's application, is a participant who is
leaving, and the word of it puts everything right. Only a step refused for a
participant who stays ends the conversation with an error — they would be
left where they should not be, and nobody would know. The refusal for one who
is leaving has not been seen on the pinned build: seventy calls ended at the
very moment of a connect never caught Asterisk between the two. What tells
the two cases apart is checked by itself, and the refusal for one who stays
is checked against Asterisk.

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
- an application that connects participants: two who then hear each other
  while the application goes on hearing each by themselves, and what it says
  to one the other does not hear; a third who, connected to one of them, is
  connected to both; one taken out of the three; one left alone because the
  other was removed, and because the other hung up — reached by the
  application as before; and one whose channel ends at the very moment they
  are being connected, which must take nobody else down;
- a step Asterisk refuses for a participant who stays — the test removes the
  bridge of their audio path behind the controller's back: the conversation
  ends with an error that names the step, and nobody is left on the line;
- the application lost while two are connected: they hear each other all
  along, and the instance that is there is handed the conversation with the
  two of them, connected, and nothing the lost instance had asked for; the
  application lost while the caller is alone, with the one instance there
  declining: the caller is hung up;
- the node without the application: a caller of the entry whose fallback is
  a transfer is called for and connected by the controller, the network
  seeing the line's number; the two are kept and, when the application is
  back, handed to it; a caller who still rings is transferred the same way,
  and when they hang up the one called for them is not kept;
- the application's side admits a connection only with the node's name and
  secret, and tells the conversation connections from the service
  connection by their paths;
- the application gives the node settings on the service connection: the
  node says which settings it stored and is welcomed; the same settings again
  change nothing; settings whose parts disagree are refused whole with the
  reason; settings with one more entry are taken in while Asterisk runs —
  Asterisk reloads without a warning, a call to the new number arrives as a
  conversation, and the settings are stored as their canonical text;
- an instance that takes a conversation connection and then says nothing at
  all is pinged at ten and twenty seconds, given up at thirty, and the caller
  is given the fallback;
- what the node does on its own is reported on the service connection, scenario
  by scenario — hang-ups, a fallback's transfer, two kept connected — and
  confirmed;
- a node without its controller: the controller is killed with two people
  connected, each with an audio path; calls that arrive then are given their
  fallbacks by Asterisk alone — one transferred through the operator, one
  hung up on; a call still waiting when a controller is started again is
  served, and the conversation from before is found, handed to the
  application with its two people connected — they heard each other all
  along — and controlled by it, while what the old controller had built for
  audio is removed and built anew under names the old one never gave; the
  new controller says it runs on the settings the application gave the old
  one, and sends again the report the old one sent and nobody confirmed.

A second, smaller test needs no Asterisk: it has the controller reach an
application over TLS that serves a certificate made for the test, given to the
controller as `SSL_CERT_FILE` the way any program on the machine is told of
roots other than the system's. With that root the controller connects,
proves the node and says hello; with another it refuses the certificate.

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
settings, adds participants by dialling, connects participants to each
other, stands in for an application that is away, and carries on the
conversations it finds in Asterisk when it is started beside it. It keeps a
service connection to the application, over TLS with the node's secret,
applies the settings it is given there while calls go on, and reports what it
does on its own. Not done yet: recording, holding a participant whose
connection is lost, prompts, requests of the application on the service
connection (starting a conversation, browser calls). A command the controller does not carry
out yet ends the conversation with an error that names it; none is accepted
and ignored. No release has been published.

## License

[Business Source License 1.1](LICENSE) — the license of the Gabion framework,
with the same parameters.

Asterisk is a registered trademark of Sangoma Technologies. This project is
NOT affiliated with, endorsed by, or sponsored by Sangoma Technologies or the
Asterisk project.
