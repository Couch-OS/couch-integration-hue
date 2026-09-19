# Couch Philips Hue integration

This repository holds the Philips Hue client for Couch: direct local control of
a Hue bridge over its v2 API (CLIP), with no Home Assistant and no Hue cloud
account. It covers link-button pairing, lights, grouped rooms, scenes, and
state kept current from the bridge's event stream.

**It is not an installable package yet, and deliberately so.** There is no
`plugin.json`, no `integration.json` and no `tests/admission.rs` here, so the
Couch integration feed cannot pin this repository. Couch's package protocol
(version 2) has no pairing a package can drive, no way for the host to store
the credential a pairing produces, no way for one connection to expose many
devices, and no light control. A Hue package built on it would be one
connection per lamp, a hand-typed application key, no certificate pin and
on/off only. The package adapter waits for these named protocol additions,
specified in [docs/protocol-needs.md](docs/protocol-needs.md):

1. pairing steps with credential write-back (shared with LG webOS, Samsung
   Tizen, Android TV and Apple TV);
2. child devices: one connection, many lights, rooms and scenes;
3. a `light` component: on/off, brightness, later colour temperature and colour;
4. versioned state polling answered from the package's event-stream cache;
5. host-run mDNS discovery (an improvement, not a blocker).

Until then the built-in Hue integration in the Couch monorepo
(`clients/couch-hue`) is the one that ships. The source here is that client,
extracted so that it builds and tests alone. Keep the two aligned until the
monorepo copy is deliberately retired. The one intended difference: the
monorepo borrows `Light` and `Command` from its Home Assistant client, and
this crate defines them itself (`src/light.rs`), with `tests/shape.rs` holding
the serialized shape Couch already stores.

## Talking to the bridge

Requests use HTTPS with a five-second deadline, no redirects, no proxy and a
4 MiB response cap. The bridge's certificate names the bridge id rather than
its address, and is issued by Signify's private CA or self-signed, so no
public root can verify it. Pairing trusts the chosen LAN bridge once, records
its exact certificate, and every later connection refuses any other one
(`couch_sdk::tls::Pin`). Handshake signatures are always verified.

`live::Live` keeps one credential-scoped session: a server-sent-events
connection to `/eventstream/clip/v2` triggers refreshes, a poll recovers when
the stream is down, and commands use their own connection so they never queue
behind the stream. An acknowledged write is held for two seconds against older
snapshots, so a dimming step cannot be undone by a read that raced it.
Unavailability always wins over a held value.

Resource ids are the bridge's v2 UUIDs: a light is its UUID, a room is
`room:<grouped_light UUID>`, a scene is `scene:<scene UUID>`.

## Build and test

The crate depends on `couch-sdk` from one exact Couch Git revision, for the
pinned-certificate verifier and the private credential writer. It does not
copy them. The lock file and the Git revision are both part of review.

```sh
cargo test --locked --all-targets
cargo build --locked --release --bin couch-hue
cargo fmt -- --check
```

The tests use local HTTP fixtures only; no bridge and no household light is
touched. They cover toggling from one fresh observation, grouped-light and
scene endpoints, Hue error envelopes inside HTTP 200, unsafe addresses and
ids, the settling guard, event-stream framing and limits, and private
credential storage. The pinned TLS path itself has no fixture in this
repository; in the monorepo it is exercised by `web/tests/hue.mjs`.

TLS is `rustls` with the `ring` provider, which compiles C and assembly, so
the ARM build needs an ARM musl C compiler. Couch supplies one (a pinned Zig
behind `tools/arm-cc-env.sh`), and the integration feed sources that same file
before it builds a package. From a Couch checkout at the pinned revision:

```sh
here=$PWD
(cd ../couch && tools/fetch-zig.sh && . tools/arm-cc-env.sh && cd "$here" && \
  cargo build --locked --release --target armv7-unknown-linux-musleabihf --bin couch-hue)
```

CI runs both: the host tests, and the static ARMv7 build with Couch's tooling
checked out at the pinned revision. CI also fails if `integration.json` or
`plugin.json` appears before the adapter work is done on purpose.

To update the Couch SDK contract, change the `couch-sdk` `rev` in `Cargo.toml`
to a reviewed full commit, regenerate `Cargo.lock`, and rerun the complete
test suite. When the package adapter is added, `couch-plugin` must use the
same revision.

## Command line

```sh
couch-hue --settings ./hue.json pair BRIDGE_IP     # press the link button first
couch-hue --settings ./hue.json lights | rooms | scenes
couch-hue --settings ./hue.json state UUID
couch-hue --settings ./hue.json on UUID | off UUID | brightness UUID PERCENT
couch-hue --settings ./hue.json recall SCENE_UUID
```

`lights`, `rooms`, `scenes` and `state` only read. The settings file holds the
application key and the pinned certificate, is written atomically with mode
`0600`, and must never be committed. Without `--settings` the path is the
remote's own, `/opt/couch/hue-connection.json`.

## Hardware status

`lights`, `rooms` and `scenes` from this extracted crate were run read-only on
2026-09-19 against one bridge through its saved pairing: the pinned
certificate was accepted and the bridge listed 48 lights, 14 rooms and 181
scenes in about one second per read. No light was switched, dimmed or
recalled from this repository, and pairing was not repeated; commands and
pairing have only fixture coverage here. The monorepo's record of the same
client is in its `docs/philips-hue.md`.

Licensed under GPL-3.0-or-later. See [LICENSE](LICENSE).
