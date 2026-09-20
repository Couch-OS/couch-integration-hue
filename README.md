# Couch Philips Hue integration

This repository holds the Philips Hue client for Couch and the installable
**package** built from it: direct local control of a Hue bridge over its v2 API
(CLIP), with no Home Assistant and no Hue cloud account. It covers link-button
pairing, lights, grouped rooms, scenes, and state kept current from the
bridge's event stream.

## What this is, and what it is not

**It is a development preview, and only that.** The package declares protocol
version 3, which is unreleased: no shipped Couch accepts its manifest, and the
official integration feed's validator accepts protocol 1 and 2 only. So:

- **the built-in Hue integration in the Couch monorepo (`clients/couch-hue`)
  is still the one that ships**, and nothing here changes or replaces it. A
  remote can run both at once; they are separate connections;
- **this repository is not pinned by the official feed**, and CI fails if it
  ever is while the package says protocol 3. The preview is installed from a
  throwaway development source instead, onto one development remote, by the
  owner, and removed the same day;
- no packaged Hue reaches a real user until the protocol 3 train ships.

## The package

One Couch connection is one bridge. Everything on the bridge is a **child** of
that connection, of one of three kinds the manifest declares:

| kind | what it is | id | control |
| --- | --- | --- | --- |
| `light` | one lamp | `<light uuid>` | on / off / toggle, `set_light` |
| `group` | one *room*'s grouped light | `room/<grouped_light uuid>` | on / off / toggle, `set_light` |
| `scene` | one scene, recalled by being told `on` | `scene/<scene uuid>` | `on` |

The connection itself does nothing: it has no capabilities and no controls of
its own. The only setting is `host`, the bridge's address on the LAN. The
application key and the bridge's exact certificate are **not** settings: they
are the connection's credential, which Couch stores privately and hands back
to the package on each start. Nothing else can read them, they are never in
the environment or in argv, and no error text this package writes contains a
key, a certificate or a path.

### Pairing

A Hue bridge issues an application key to whoever asks for one while its round
button is being pressed, and before that it answers error 101. So the whole
conversation is one request repeated: `POST /api` with
`{"devicetype":"couch#package-dev","generateclientkey":false}`, once per poll,
until it stops saying 101. Couch draws the dialog; the package only says what
to wait for.

Three things make it more than a loop.

- **The certificate is trusted exactly once, and only then.** A bridge's
  certificate names the bridge id rather than its address and is issued by
  Signify's private CA or self-signed, so nothing public can verify it. The
  only honest moment to decide to trust one is while a person is standing at
  the bridge pressing its button - so the first handshake of a pairing fills
  an empty pin cell, every later request in that conversation must present
  exactly that certificate, and it is stored in the credential beside the key.
  A bridge that presents a different one half way through ends the
  conversation, and says so.
- **The clock belongs to the package.** The manifest gives the dialog 120
  seconds; the flow gives up at 115, so the person is told "not linked in
  time" rather than watching a dialog vanish. A request that times out does
  **not** end it: a bridge under load is still a bridge, and only one that
  cannot be reached at all ends the conversation early.
- **The key is proved before it is handed over.** After the bridge issues one,
  the flow reads `/api/0/config` for the bridge id and then makes one
  authenticated read through the pinned client. A pairing that hands Couch a
  key it has never used is a pairing that fails later, in front of somebody
  who has already walked away.

Re-pairing starts clean. Hue issues a new key every time the button is
pressed and has no notion of one key standing for another, so the key Couch
already holds is ignored; the old one stays valid on the bridge until its
owner deletes it in the Hue app, where it is listed as `couch#package-dev`.

Every line a conversation can produce is written out in `src/pairing.rs`, and
the only thing in any of them that comes from the bridge is the last six
characters of its id.

### Limits of this preview

- **No colour.** `xy` is refused. Colour temperature (mirek) is supported on a
  lamp that reports a `mirek_schema`, and clamped to that lamp's range.
- **Rooms, not zones.** A Hue *zone* becomes no `group` child in 0.1.0; its
  scenes are still listed, with the zone's name as their room hint.
- **State is a cache, refreshed from the bridge's event stream.** A row on the
  remote follows a change made in the Hue app within about five seconds, where
  built-in Hue follows it in about half a second. Live state pushed from a
  package is a later step; until then the panel re-reads on its own round.
- **A vanished or unreachable child stays listed as unavailable** rather than
  disappearing: its state is unknown, never an inferred "off".
- **A room takes one command a second**, which is the bridge's limit for a
  grouped light; sent more, it drops them silently and leaves the room at a
  level nobody asked for. So a room's writes are **coalesced** rather than
  refused: every one is answered at once with the state it asks for, at most
  one command a second reaches the bridge, and the newest target replaces one
  still waiting. A held brightness key therefore produces two bridge commands
  a second and no refusals, instead of an error on every other step. Nothing
  is lost in the other direction either: a write this package answered and
  then could not send drops its optimistic state and is reported, once, to
  whoever asks next. A `busy` variant in `couch_sdk::Error` would still be the
  cleaner long-term answer for a host that wants to re-queue rather than be
  told "done" early, but no core change is needed for this to behave.
- **Single lamps are not coalesced.** Hue's guidance is about ten commands a
  second for a light against one for a group, and a held key on the remote is
  nowhere near ten a second, so a lamp's writes go straight out.

### Versions

`abuild` cannot spell a pre-release with a hyphen and Cargo cannot spell one
without, so the same version has two spellings:

| where | spelling |
| --- | --- |
| `Cargo.toml` | `0.1.0-pre2` (semver) |
| `plugin.json`, the APK, the feed | `0.1.0_pre2` |

`tools/integrations/build-apk.sh` checks that `plugin.json`'s version equals
the APK version exactly, so `plugin.json` carries the underscore spelling.
Every rebuild of a preview bumps the suffix (`_pre2`, ...): published bytes are
immutable.

### Why there is no `tests/admission.rs`

The curated feed greps an integration's `tests/admission.rs` for four literal
cases (`testing::conformance(`, `testing::failure(`, `testing::timeout_no_retry(`,
`testing::spike(`). None of them can pass for this package, and not because of
anything here: `couch_plugin::testing::Package::endpoint` starts the package
with no credential, and a package whose manifest says `pairing.required`
answers `unpaired` to everything without one. The harness has no way to hand
one over.

That is a core gap, recorded as G1 in the protocol 3 plan, and it is fixed at
step T7 together with the feed's protocol check. **Publishing this package to
the official feed waits for both**: T7, and a core change that lets an
admission case start a paired package. Until then the cases that can be
written are written, against a fake bridge, in this repository's own tests.

## Talking to the bridge

Requests use HTTPS with short deadlines, no redirects, no proxy and a 4 MiB
response cap. The bridge's certificate names the bridge id rather than its
address, and is issued by Signify's private CA or self-signed, so no public
root can verify it. Pairing trusts the chosen LAN bridge once, records its
exact certificate, and every later connection refuses any other one
(`couch_sdk::tls::Pin`). Handshake signatures are always verified.

A status read never becomes a request. Couch asks a packaged connection for a
child's state on every round of an open panel, so the bridge is read by
background threads and a read is a lookup in a cache: answered in under a
millisecond, and proved against the fake bridge's own request log. The first
read of a child process has nothing to look up, so it waits up to a second for
the first snapshot and then answers "unknown" - never "off". A snapshot that
has gone stale (65 seconds with the event stream up, 5 without) is reported as
the failure that made it stale, and never served.

One credential-scoped session keeps a server-sent-events connection to
`/eventstream/clip/v2` open and refreshes a cache from it; a poll recovers when
the stream is down; writes use their own connection so they never queue behind
the stream. An acknowledged write is held for two seconds against older
snapshots, so a dimming step cannot be undone by a read that raced it.
Unavailability always wins over a held value.

## Build and test

The crate depends on `couch-plugin` and `couch-sdk` from one exact Couch Git
revision, for the package server, the pinned-certificate verifier and the
protocol types. It does not copy them. The lock file and the Git revision are
both part of review.

```sh
cargo test --locked --all-targets
cargo build --locked --release --bin couch-plugin-hue
cargo fmt -- --check
```

### Nothing here talks to a real bridge

**Every test in this repository connects to one fake bridge on 127.0.0.1, and
to nothing else.** It is in `tests/fake/bridge.rs`: a rustls server with its
own self-signed certificate whose common name is its bridge id, serving CLIP
v2 over a generated household of 48 lights, 14 rooms, 2 zones and 181 scenes,
with the devices, zigbee links and grouped lights behind them. Writes change
it and the next read agrees; a wrong key is a 401; an event stream reports the
change afterwards; `silent()` answers nothing while staying connected and
`close()` stops being a bridge at all; and it logs every request that reached
it, which is how a test proves a status read touched no network.

No test discovers anything, browses mDNS, or sends a byte to any address but
the loopback port that fake chose. The package trusts the fake only because
its certificate arrived inside a credential the test built: there is no
test-only trust bypass anywhere in this repository, in test code or shipping
code.

TLS is `rustls` with the `ring` provider, which compiles C and assembly, so
the ARM build needs an ARM musl C compiler. Couch supplies one (a pinned Zig
behind `tools/arm-cc-env.sh`), and the integration feed sources that same file
before it builds a package. From a Couch checkout at the pinned revision:

```sh
here=$PWD
(cd ../couch && tools/fetch-zig.sh && . tools/arm-cc-env.sh && cd "$here" && \
  cargo build --locked --release --target armv7-unknown-linux-musleabihf --bin couch-plugin-hue)
```

CI runs both: the host tests, and the static ARMv7 package build with Couch's
tooling checked out at the pinned revision. CI also fails if `integration.json`
stops saying protocol 3, or if the official feed pins this repository.

To update the Couch SDK contract, change the `rev` in `Cargo.toml` to a
reviewed full commit (`couch-plugin` and `couch-sdk` must share it),
regenerate `Cargo.lock`, and rerun the complete test suite. The rule for a
preview is stricter: **the pin must be the commit the preview runtime was
built from.**

## Command line

The crate also builds a small read-mostly command line, `couch-hue`, which
talks to a bridge through a settings file of its own. It is a development
tool: it is not what the package runs, and the package never reads a file.

```sh
couch-hue --settings ./hue.json pair BRIDGE_IP     # press the link button first
couch-hue --settings ./hue.json lights | rooms | scenes
couch-hue --settings ./hue.json state UUID
couch-hue --settings ./hue.json on UUID | off UUID | brightness UUID PERCENT
couch-hue --settings ./hue.json recall SCENE_UUID
```

`lights`, `rooms`, `scenes` and `state` only read. The settings file holds the
application key and the pinned certificate, is written atomically with mode
`0600`, and must never be committed.

Nothing under `src/` writes to stdout. The package executable's stdout is the
protocol socket Couch reads framed JSON from, and one stray byte on it costs
the connection its child process; the command line's own printing is in
`src/main.rs`, which the package never runs.

## Hardware status

`lights`, `rooms` and `scenes` from the extracted client were run read-only on
2026-09-19 against one bridge through its saved pairing: the pinned
certificate was accepted and the bridge listed 48 lights, 14 rooms and 181
scenes in about one second per read. No light was switched, dimmed or
recalled from this repository, and pairing was not repeated.

**The package has never spoken to a real bridge.** Everything it does is
proved against a fake CLIP v2 bridge on the loopback interface, and the first
real run is a supervised session on the owner's development remote.

Licensed under GPL-3.0-or-later. See [LICENSE](LICENSE).
