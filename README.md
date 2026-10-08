# asterisk-controller

The program that runs next to Asterisk® on a
[Gabion](https://github.com/gabion-dev) telephony node and controls it on
behalf of a Gabion application. It is part of the Gabion framework and is
licensed with it.

## What it is

A telephony node is Asterisk and this controller on one machine. The
controller writes Asterisk's whole configuration, connects to Asterisk's
control interface, and for every call opens a connection to the application.
It translates between Asterisk and the Gabion node protocol, which is
described in `protocol/`. When the application cannot be reached, the
controller carries out what the application's settings say for that case
(see [What happens to calls](#what-happens-to-calls)).

## What the node accepts and where it connects

From the network a node accepts only what Asterisk accepts:

- SIP from the telephone operators named in the node's settings, at the
  address `--sip` gives, over UDP or TCP as each operator's transport says.
  Asterisk recognises an operator only by the address its calls come from; a
  call from any other address is refused. Without operators in the settings
  the controller configures no SIP transport;
- RTP call audio, on the UDP ports `--audio-ports` gives.

Nothing on the node accepts commands from the network:

- the controller listens only on the loopback address (`--listen`), for
  Asterisk's audio connections;
- Asterisk's HTTP server, which carries its control interface, listens only
  on the loopback address (`--asterisk-http`);
- Asterisk's management interface (AMI) is turned off.

The node opens its outside connections itself:

- the controller connects to the application at the address `--application`
  gives: one WebSocket connection for each conversation
  (`<address>/conversation`) and one standing service connection
  (`<address>/service`). For each prompt it needs it also makes a one-off
  `GET <address>/prompts/<identifier>` — over HTTPS when the address is
  `wss://`, over plain HTTP when it is `ws://`;
- Asterisk connects to the operators, for outbound calls and transfers.

The application needs no open port towards the node. Every connection to it
carries the node's name and secret as HTTP Basic authentication. On every
WebSocket connection each side sends a ping at least every 10 seconds, and
30 seconds with nothing from the other side counts as a lost connection;
opening a connection is held to the same 30 seconds. A lost service
connection is opened again a second later.

## Running a node

You need:

- the controller binary — no release is published yet, so build it from
  source ([below](#building-and-checking-from-source));
- an Asterisk 22.11.0 tree. The controller drives only that version: it reads
  the version and the list of modules from `<tree>/BUILD-INFO.txt` and does
  not start with any other version, or without that file.
  `scripts/fetch-asterisk.sh` fetches the prebuilt tree the controller is
  checked against — release `22.11.0-r1` of
  [gabion-dev/asterisk-server](https://github.com/gabion-dev/asterisk-server),
  for Linux (x86_64, aarch64) and macOS (arm64, x86_64) — checks its pinned
  SHA-256 and unpacks it into `target/asterisk-server`. The tree is only read;
- a state directory for the node, with the node's secret in it
  ([below](#the-state-directory)).

### Starting

Start the controller first:

```sh
asterisk-controller --node <name> --listen 127.0.0.1:<port> \
    --application <wss://host[:port][/path]> \
    --asterisk-tree <directory> --state <directory> \
    --asterisk-http 127.0.0.1:<port> --audio-ports <first>-<last> \
    [--sip <address:port>] [--sip-public <address>]
```

- `--node` — the node's name, as the application knows it: 1 to 128
  characters, each an ASCII letter, a digit, `.`, `_` or `-`.
- `--listen` — where the controller accepts Asterisk's audio connections.
  A loopback address only.
- `--application` — the address of the application: `wss://host[:port][/path]`,
  without `?` or `#`. `ws://` is accepted only for a loopback address or
  `localhost`.
- `--asterisk-tree` — the Asterisk tree.
- `--state` — the node's state directory. Its path may not hold control
  characters or any of `, & ; $ ( ) [ ] { } \ " ' |`. Keep it short:
  Asterisk's console socket, `<state>/run/asterisk.ctl`, must fit the limit
  of a Unix socket path — 108 bytes on Linux, 104 on macOS. With a longer
  path Asterisk runs, but `asterisk -rx` cannot connect to it (seen on
  Linux); the controller does not check the length.
- `--asterisk-http` — where Asterisk's HTTP server and control interface
  listen; the controller writes it into Asterisk's configuration and
  connects there. A loopback address only.
- `--audio-ports` — the UDP ports Asterisk may use for call audio, first and
  last. The first must be even and not 0, the last odd.
- `--sip` — where Asterisk listens for operators' SIP. Needed only when the
  settings name an operator: with such settings stored and no `--sip`, the
  controller does not start. A new `--sip` takes effect only when Asterisk
  is started again — a running Asterisk kept listening on the old port when
  its configuration was reloaded (seen in a test on one machine with
  Asterisk 22.11.0).
- `--sip-public` — the IP address operators reach the node at, when it
  differs from the one in `--sip` (a machine behind address translation).
  It is written into Asterisk's SIP transports as their external signalling
  and media address.

An unknown or missing argument, or one without a value, stops the controller
with the usage text.

Before it says it is ready, the controller checks the Asterisk version, reads
the stored settings, makes or reads the control secret, rewrites
`<state>/etc` whole, reads the node's secret and — for a `wss://` address —
the machine's trusted root certificates, and starts listening on `--listen`.
If any step fails it says why on standard error and exits with status 1.
When it is ready it prints one line on standard output:

```
asterisk-controller ready on <the --listen address>
```

Then start Asterisk on the configuration the controller wrote, and on nothing
else. The checks start it this way, on Linux with `LD_LIBRARY_PATH` set:

```sh
LD_LIBRARY_PATH=<tree>/lib <tree>/sbin/asterisk -C <state>/etc/asterisk.conf -f -n
```

`-f` keeps Asterisk in the foreground and `-n` turns off console colours;
Asterisk prints `Asterisk Ready.` when it has started. The controller finds
Asterisk by itself, trying every 250 milliseconds, and finds it again whenever
the connection is lost.

`<state>/etc` is rewritten whole on every start of the controller: do not
edit it.

### Settings

What the node does with calls — its operators, outbound lines, entries
(dialled numbers) with their fallbacks, and prompts — is its settings: the
`Settings` of `protocol/node-protocol.schema.json`. The application gives
them on the service connection. The node applies them while it runs — it
rewrites the dialplan and the operators' file and has Asterisk reload those
two — and keeps the last settings it applied in `<state>/settings.json`, on
which it starts next time. Settings it cannot take it refuses with the
reason, and keeps the ones it had.

Without `settings.json` the node has no settings, and no call enters or
leaves it. You may put a `settings.json` there yourself: the controller
starts on any file that passes the protocol description and the node's own
checks, and does not start with one that fails them.

### The state directory

You put there:

- `node-secret` — the node's secret, readable by its owner alone; give the
  application the same secret for the node's name. Leading and trailing
  white space is ignored. Without the file, with an empty one, or with one
  that others may read, the controller does not start;
- optionally `settings.json` (above).

The controller writes:

- `control-secret` — the password of the user `gabion-controller` of
  Asterisk's control interface, made once for the state directory and kept,
  mode 0600. A controller started next to a running Asterisk must present
  the password that Asterisk read when it started: keep this file while
  Asterisk runs;
- `etc/` — every configuration file of Asterisk, mode 0700, rewritten on
  every start;
- `settings.json` — the settings last applied;
- `reports.jsonl` — reports of what the node did on its own that the
  application has not yet confirmed;
- `prompts/<sha256>.sln16` — the audio of prompts, fetched from the
  application and checked against their SHA-256; a prompt the settings no
  longer name is removed.

It also creates `db/`, `keys/keys/`, `spool/`, `run/` and `log/` for
Asterisk, and names them in `etc/asterisk.conf`.

The state directory holds secrets: the control password in `control-secret`
and `etc/ari.conf`, the operators' passwords in `etc/pjsip.conf` and
`settings.json`. Keep the state directory accessible to the node's user only
(for example, `chmod 700`).

A file that changes while the node runs is written under a temporary name
and renamed, so it is never found half written; `reports.jsonl` is appended
to.

### Output

Neither the controller nor Asterisk writes a log file: whoever starts them
keeps their output. The controller writes its ready line to standard output
and everything else to standard error, each line beginning with
`asterisk-controller:` or `conversation <id>:`. Asterisk writes to its
console only, at the levels notice, warning and error.

Lines that say the node is working:

- `asterisk-controller: settings <fingerprint> are applied: <n> operators, <n> lines, <n> entries`,
  or `asterisk-controller: the node has no settings yet: no call enters or leaves it`;
- `asterisk-controller: Asterisk 22.11.0 is to be started with <state>/etc/asterisk.conf`;
- `asterisk-controller ready on <address>`;
- `asterisk-controller: Asterisk is not there yet — <reason>` — until
  Asterisk is started;
- `asterisk-controller: connected to Asterisk`;
- `asterisk-controller: the application welcomed this node`.

Lines that say something is wrong:

- `asterisk-controller: the connection to Asterisk is LOST` — Asterisk
  stopped, or the connection to it broke;
- `asterisk-controller: a REPORT IS LOST — <reason>` — a report could not be
  written, and the application will not learn what the node did;
- `asterisk-controller: no service connection — <reason>` — the application
  cannot be reached, or it refused the node;
- `asterisk-controller: settings <fingerprint> are refused — <reason>`;
- `asterisk-controller: prompt "<identifier>" is not here; …` — until it is
  fetched, a message fallback plays nothing;
- `conversation <id>: FAILED — <reason>` — a conversation could not go on.

While the controller keeps trying to reach Asterisk, the application or an
instance for a conversation, it says the reason once, and again only when the
reason changes.

## What happens to calls

### When the application cannot be reached

This is when no instance of the application takes a new conversation — none
can be reached, the one reached declines, or it says nothing for 30 seconds —
or when the instance that had a conversation is lost. Then:

- people connected to each other stay connected. The controller looks for an
  instance every second, and the instance that takes the conversation gets
  it as it is now — who is in it, in what state, who is connected to whom —
  and nothing of what the lost instance had asked for;
- a caller who was alone with the application is given the fallback of their
  entry from the settings. A transfer: the controller calls the fallback's
  number on the fallback's line, showing the line's number, and connects the
  caller to whoever answers; if the number cannot be dialled, or nobody
  answers, the caller is hung up. A message: the prompt is played to the
  caller, and the call ends;
- anyone else left with nobody to talk to is hung up.

The node reports each of these to the application on the service connection.
A report is kept in `reports.jsonl` until the application confirms it, and is
sent again, under the same identifier, after a lost connection or a restart
of the controller.

### When the controller is away

When the controller is not running, or not connected to Asterisk, Asterisk
acts alone on what the controller wrote:

- a new call tries to enter five times, a second apart — about four seconds.
  A controller that is back by then serves it. Otherwise Asterisk carries out
  the entry's fallback itself: a transfer on the line's operator, showing the
  line's number, or the prompt and the end of the call;
- calls already in progress stay in Asterisk, and people connected to each
  other go on talking. Audio between the application and participants stops
  until the controller is back.

Restarting the controller is up to whoever runs the node.

### When the controller is started again

A controller started next to a running Asterisk finds the calls left in it and
offers each conversation to the application as it is now. What the controller
before it had built for audio is removed and built anew when the application
asks; reports it left unconfirmed are sent again.

### When Asterisk stops

When Asterisk stops — on SIGTERM or SIGINT it hangs up every call before it
exits — the node's calls end. The controller says `LOST`, tells the
application, on the connection of each conversation an instance owns, that
every participant has left and the conversation has ended, and looks for
Asterisk again every 250 milliseconds.

### Other cases

- The controller warns of a missing prompt when it starts, and fetches it
  when the application welcomes the node.
- When the application breaks the protocol, the conversation ends.

## Stopping

On SIGINT the controller exits with status 0. It catches no other signal:
SIGTERM ends it by the signal's default action. Either way nothing is cleaned
up — calls stay in Asterisk, people connected to each other go on talking,
and the next controller carries the calls on; reports not yet confirmed stay
in `reports.jsonl`.

The controller never starts or stops Asterisk. Stopping Asterisk ends every
call of the node.

## Building and checking from source

The machine needs:

- Rust as pinned in `rust-toolchain.toml` (1.99.0, with clippy and rustfmt);
  rustup installs it on first use;
- a C compiler: `ring`, the cryptography library of the controller's TLS,
  compiles C code;
- for `scripts/fetch-asterisk.sh`: bash, curl, tar, `sha256sum` or `shasum`,
  and access to github.com;
- for the tests: the Asterisk tree (from the script, or `ASTERISK_TREE`
  pointing at one) and `127.0.0.2` as an address of the machine; the path of
  the clone may not hold the characters `--state` refuses, because the tests
  keep a state directory under `target/`;
- for the Java side: a JDK 21, and `cargo` on the `PATH` — the Java message
  types are generated by `cargo run -p protocol-java`. The Gradle wrapper
  downloads Gradle 9.3.0 and the libraries, which are checked against pinned
  checksums.

```sh
cargo build --locked -p asterisk-controller   # target/debug/asterisk-controller
cargo fmt --all --check
cargo clippy --all-targets --locked
scripts/fetch-asterisk.sh
cargo test --locked
(cd java && ./gradlew check)
```

The controller is checked against a real Asterisk — the tree the script
fetches — on one machine. The test starts the controller, then Asterisk on
the configuration the controller wrote, which Asterisk must load without a
single warning or error. A second Asterisk plays the telephone network: the
operator at `127.0.0.1` and a stranger at `127.0.0.2`. The test itself plays
the application, over `ws://` on the loopback address. Without an Asterisk
tree the test fails and says how to get one. A separate test, without Asterisk,
has the controller reach an application over TLS with a certificate made for
the test.

`.github/workflows/checks.yml` runs the same checks on every push and pull
request.

## Status

Early development; no release has been published. Not built yet: recording;
holding a participant whose audio is lost; the application's requests on the
service connection; calls from user endpoints and web passes; TLS towards an
operator and operators that require registration.

## License

[Business Source License 1.1](LICENSE) — the license of the Gabion framework,
the same file. The Licensed Work it names, Gabion Framework, includes this
controller.

The Asterisk name and logos are trademarks owned by Sangoma US Inc. This
project is not affiliated with, endorsed by, or sponsored by Sangoma or the
Asterisk project.
