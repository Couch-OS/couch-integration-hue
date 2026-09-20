# What a Hue package needs from Couch

> **Status, 2026-09-20.** Protocol 3 exists, unreleased, and this repository
> now holds the package adapter built on it: `plugin.json`, `integration.json`
> and `src/adapter.rs`, with pairing still to come. Sections (i) to (iv) below
> are what was asked for and what was built; read them as the record of why
> protocol 3 has the shape it has, not as a list of things that are missing.
> Two are still open: an admission case cannot hand a credential to a package
> that requires pairing, and the feed's metadata check still reads the literal
> protocol 2. Both are for step T7; see the README.

This note was written when this repository held the Hue client and no package
adapter, because the package protocol as of Couch `00ab4da` (protocol version
2) could not carry what built-in Hue does. This note says exactly what is missing and proposes the additions,
as protocol version 3, concretely enough to implement. Sections (i) and (iv)
are deliberately generic: the same pairing and state mechanisms serve LG
webOS, Samsung Tizen, Android TV and Apple TV.

Paths are in `Couch-OS/couch`. **[frozen]** marks a path listed under
`core.contract_paths` in `tools/release/tested-integrations.json`; changing it
means renewing the tested-integrations evidence. `clients/couch-plugin/src/testing.rs`
is additionally hash-pinned under `core.harness_paths`.

## Why there is no preview package today

What built-in Hue does (`clients/couch-hue`, `daemon/couch-confd/src/api/hue.rs`,
`ui/couch-gui/src/lights.rs`, `ui/couch-gui/src/connections.rs::HueFleet`,
`web/couch-web/src/screens/device_picker.rs`):

- pairs with the link button and stores an application key plus the bridge's
  exact certificate, privately, per connection;
- exposes one bridge as many Couch devices: on a real bridge read during this
  work, 48 lights, 14 rooms and 181 scenes from one connection;
- toggles and dims lights and grouped rooms with optimistic targets, and
  recalls scenes from a room's Scenes button;
- keeps state current from the bridge's event stream, with polling as recovery.

What protocol 2 offers (`clients/couch-plugin/src/protocol.rs`):

```rust
pub enum Request  { Hello, Configure { settings }, Command { function }, Action { action }, Status, Inputs }
pub enum Response { Hello { manifest }, Ok, Status { status }, Inputs { inputs }, Error { code } }
```

| Need | Protocol 2 | Evidence |
| --- | --- | --- |
| Pairing | None. Settings flow host to child only; no response carries data to store. | `Response` has no such variant; `daemon/couch-confd/src/plugins.rs::execute` forwards only `Command`, `Action`, `Status`, `Inputs`. |
| Stored credential | Only user-typed `secret` text. The pinned certificate has nowhere to live, so trust-on-first-use would repeat on every child start, which is no pin at all. | `manifest.rs::FieldKind`, `SettingField::accepts` (text, 4096 chars, typed by a person). |
| Many devices per connection | None. `Integration::Plugin.resource_id` exists in the model and is validated, but is never sent to the child; the GUI passes `resource_id: String::new()`. | `model/couch-model/src/device.rs`, `validate.rs:260`, `ui/couch-gui/src/activity_buttons.rs:1548`, `api/plugins.rs::plugin_route`. |
| Brightness | None. `TypedAction` has one variant, `SetVolumeDb`; a manifest may declare at most one action; `Status` has no brightness. `dim:N` parses as a function but each level would have to be a separately declared capability. | `model/couch-model/src/volume.rs`, `manifest.rs::validate` (`self.actions.len() > 1`), `couch-sdk/src/status.rs`. |
| Appearing as a light | None. A packaged device in a room opens the core control screen (`plugin:<device_id>`, made for TVs and receivers), not a light row with a level. | `ui/couch-gui/src/lights.rs::configured_in`, `tv_connection`, `tv_plugin.rs`. |
| Live state | None. The child never speaks first. | `server.rs::serve` is a strict request/response loop. |

A package built on protocol 2 would therefore be: one connection per room or
lamp, an application key the owner obtains by hand with `curl` and types in
once per connection, no certificate pin, on/off only, shown on a TV-shaped
screen. That is a toy next to the built-in integration, so none is published.
`docs/integration-architecture.md` in Couch reaches the same verdict for
Home Assistant and Hue: "needs discovery, pairing, and typed domain controls".

## (i) Pairing a package can drive, with credential write-back

One mechanism for every device that needs the owner's approval. The package
describes steps; Couch renders them natively (browser and panel) and stores
the result. A package never supplies UI and never writes files.

### Settings and credential are separate things

- **settings**: what a person types (bridge address). Declared in the manifest
  as today.
- **credential**: an opaque JSON object the package produced during pairing
  (Hue: application key, pinned certificate, bridge id. webOS: client key and
  certificate. Android TV: client certificate, private key, server
  certificate. Apple TV: HAP/Companion long-term keys). Never typed, never
  shown, never returned over HTTP, excluded from configuration export, deleted
  with the connection.

Manifest addition:

```json
"pairing": { "required": true, "max_seconds": 120 }
```

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    /// Commands are refused with Error::Unpaired until a credential exists.
    pub required: bool,
    /// Host-enforced ceiling for one pairing session, 10..=300.
    pub max_seconds: u16,
}
// Manifest gains: #[serde(default)] pub pairing: Option<Pairing>   (protocol >= 3 only)
```

### Messages

```rust
pub enum Request {
    // ... existing ...
    /// `credential` is None until paired. Replaces the v2 shape for v3 children.
    Configure { settings: Value, #[serde(default)] credential: Option<Value> },
    /// Begin. Needs no prior Configure: the settings here are the unsaved
    /// form contents. May contact the device (unlike Hello and Configure).
    PairStart { settings: Value },
    /// Poll (input: None) or answer a prompt.
    PairContinue { session: String, #[serde(default)] input: Option<PairInput> },
    PairCancel { session: String },
}
pub enum Response {
    // ... existing ...
    Pairing { session: String, step: PairStep },
}

#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairStep {
    /// Show `prompt`; call PairContinue again after `poll_after_ms`
    /// (0 = only when the person has answered).
    Waiting { prompt: PairPrompt, poll_after_ms: u32 },
    /// Finished. The host stores `credential`, then saves `settings` (the
    /// package may have normalised them, e.g. added the bridge id).
    Done { credential: Value, settings: Value, summary: String },
    Failed { reason: PairFailure },
}
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairPrompt {
    /// Hue link button. The host polls.
    PressButton { message: String },
    /// "Allow" on the TV: LG webOS, Samsung Tizen. The host polls.
    ApproveOnDevice { message: String },
    /// The device shows a code the person types into Couch: Android TV
    /// (6 hex), Apple TV (4 digits).
    EnterCode { message: String, length: u8, alphabet: CodeAlphabet },
}
pub enum CodeAlphabet { Digits, Hex, Alphanumeric }
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairInput { Code { code: String } }
pub enum PairFailure { Unreachable, Refused, WrongCode, TimedOut, Unsupported }
```

Wire example, Hue:

```json
→ {"id":2,"body":{"method":"pair_start","settings":{"host":"192.168.1.20"}}}
← {"id":2,"body":{"type":"pairing","session":"p1","step":{"step":"waiting",
     "prompt":{"kind":"press_button","message":"Press the round link button on your Hue Bridge"},
     "poll_after_ms":2000}}}
→ {"id":3,"body":{"method":"pair_continue","session":"p1"}}
← {"id":3,"body":{"type":"pairing","session":"p1","step":{"step":"done",
     "credential":{"application_key":"…","certificate":"<base64 DER>","bridge_id":"…"},
     "settings":{"host":"192.168.1.20"},"summary":"Paired; 48 lights found"}}}
```

Android TV differs only in the middle: `waiting` carries `enter_code`, the
next `pair_continue` carries `{"kind":"code","code":"A1B2C3"}`.

### Host rules

- The pairing child is the connection's one child; while a session is open the
  host sends it nothing but `PairContinue`/`PairCancel` and does not reap or
  restart it (Android TV's pairing TLS session must stay open between the two
  steps). For a connection that does not exist yet, the host spawns the child
  under a temporary id and creates the connection on `Done`.
- Each `PairContinue` keeps the ordinary 12 s request deadline; a package
  answers a poll in about 2 s. The host ends the session at `max_seconds`,
  sends `PairCancel`, and kills the child if that is not answered.
- `credential` is at most 16 KiB (a frame is 64 KiB and must also hold the
  settings). The host writes it with `couch_sdk::save_private` (atomic, 0600)
  to `connections/<id>/plugin-credential.json`, next to the existing private
  settings file, under the same per-connection lock, before it saves
  `settings`. A failed pairing leaves the previous credential untouched
  (built-in Hue's promise today).
- Rotation outside pairing (a webOS client key can be reissued): any v3
  response envelope may carry `"store_credential": {…}` beside `id` and
  `body`; the host persists it before returning the body to the caller. Hue
  does not need this.
- New error code `Unpaired`, so the UI can say "pair this connection" instead
  of "could not be reached".

### HTTP and UI

- `POST /api/connections/<id>/plugin/pair` `{settings}` → `{session, step}`
- `POST /api/connections/<id>/plugin/pair/<session>` `{input?}` → `{step}`
- `DELETE /api/connections/<id>/plugin/pair/<session>`
- `GET …/plugin/settings` gains `"paired": true|false`; the credential itself
  is never returned.

The browser renders the three prompt kinds with its own components and polls
on `poll_after_ms`. The panel can render the same steps later; nothing in the
protocol is browser-specific.

### Files

| File | Change |
| --- | --- |
| `clients/couch-plugin/src/protocol.rs` **[frozen]** | `PROTOCOL_VERSION = 3`; new requests, responses, `Error::Unpaired`, optional `store_credential` on the envelope. |
| `clients/couch-plugin/src/manifest.rs` **[frozen]** | `pairing`; validate `max_seconds`; refuse on protocol < 3. |
| `clients/couch-plugin/src/server.rs` **[frozen]** | Route pairing to the client; pass `credential` to `connect`. |
| `clients/couch-plugin/src/host.rs` **[frozen]** | Session pinning: no reap or restart while pairing; `max_seconds`. |
| `clients/couch-plugin/src/testing.rs` **[frozen, hash-pinned]** | A fifth shared admission case, `pairing(`: refused, timed out, cancelled, oversized credential, credential never echoed in a later response. |
| `clients/couch-sdk/src/client.rs` **[frozen]** | `trait Pair: DeviceClient { type Credential; fn pair_start(..); fn pair_continue(..); }` and `connect(settings, credential)`. |
| `daemon/couch-confd/src/plugins.rs` **[frozen]** | Credential file, lock, size cap, redaction, deletion with the connection. |
| `daemon/couch-confd/src/api/plugins.rs` **[frozen]** | The three routes; `paired` in the settings view. |
| `web/couch-web/src/screens/connections.rs` | Native pairing dialog for a packaged connection. |
| `tools/release/tested-integrations.json` | `supported_protocol_versions: [1, 2, 3]`. |
| `couch-integrations: scripts/validate_feed.py` | `PROTOCOL_VERSIONS`, and `pairing(` in `REQUIRED_ADMISSION_CALLS` for manifests that declare pairing. |

## (ii) Child devices: one connection, many devices

The saved configuration already has the right shape and needs no migration of
its own: a device stores `{"via":"connection","connection_id":…,"resource_id":…}`
and `Integration::Plugin` carries `resource_id`. What is missing is that the
resource never reaches the child, and that a package cannot list its resources.

### Listing

Manifest: `"children": [ChildKind, …]` (at most 8), each naming what a child of
that kind can do, so that Couch can validate bindings without the package
running (the same reason `Provider::Plugin` snapshots capabilities today):

```json
"children": [
  {"kind":"light","label":"Light","device_kind":"light","component":"light","capabilities":["on","off","toggle"]},
  {"kind":"group","label":"Hue room or zone","device_kind":"light","component":"light","capabilities":["on","off","toggle"]},
  {"kind":"scene","label":"Hue scene","device_kind":"other","component":"scene","capabilities":["on"]}
]
```

```rust
pub enum Request  { /* … */ Children { #[serde(default)] cursor: Option<String> } }
pub enum Response { /* … */ Children { children: Vec<Child>, #[serde(default)] next: Option<String> } }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    /// Stable for the life of the pairing. Must satisfy validate.rs's plugin
    /// resource rule: <= 128 bytes of [A-Za-z0-9._/+-].
    pub id: String,
    /// One of the manifest's `children[].kind`.
    pub kind: String,
    pub name: String,
    /// The device's own idea of where it is ("Office"). A suggestion only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_hint: Option<String>,
    /// Per-device traits for the `light` component; see (iii).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<LightTraits>,
}
```

Paging is required, not a nicety: the real bridge above lists 243 children;
as compact `Child` records that is about 25 KB, and built-in Hue's equivalent
JSON is 52 KB, against a 64 KiB frame. At most 64 children per frame.

Hue ids: a light is its v2 UUID (unchanged from built-in). Built-in uses
`room:<uuid>` and `scene:<uuid>`, and `:` is not in the plugin resource
alphabet, so a package uses `room/<uuid>` and `scene/<uuid>`; the conversion
of an old Hue connection rewrites the prefix. Hue v2 UUIDs survive renames and
bridge restarts; they change only if the light is deleted and re-added.

### Addressing

```rust
Command { function: String, #[serde(default)] resource: Option<String> },
Action  { action: TypedAction, #[serde(default)] resource: Option<String> },
Status  { #[serde(default)] resource: Option<String> },
```

`None` keeps today's meaning (the connection is the device). With `children`
declared, the host refuses a request whose resource is empty, and gates
`function` against the capabilities of that child's kind (kind is stored with
the device, below), before the child is asked anything.

`LocalRequest` (GUI to daemon over `plugin.sock`) gains `resource_id`; the
HTTP routes gain `?resource=`; `GET /api/connections/<id>/plugin/children`
returns the joined pages.

### How they appear in rooms

Exactly as built-in Hue does now, because that flow is already right:

1. **Rooms & devices → Add devices to this room →** the packaged connection.
   The picker (`device_picker.rs`, the `discover` branch, today limited to
   UniFi Protect, Hue, Home Assistant and Matter) lists `children`, with a
   kind selector from the manifest's labels, a filter by `room_hint`, and
   search. Nothing is assigned automatically.
2. When the Couch room's name matches a `room_hint`, that filter is
   preselected. "Import this room" (add every child with that hint) stays.
3. A chosen child becomes a `Device` with `kind` = the child kind's
   `device_kind`, integration `{"via":"connection", connection_id, resource_id}`.
   `Integration::Plugin` gains `child_kind: String` so the panel knows the
   component without the package running.
4. A child whose component is `scene` is not a row: it is attached to the
   room's Scenes button. `Scene.hue: Option<HueScene>` generalises to
   `Scene.resource: Option<SceneResource { connection_id, resource_id }>`,
   reading the old `hue` key as an alias.
5. In the panel's room list a child whose component is `light` is a light row
   (`lights.rs::configured_in` gets a `Plugin` arm beside `Hue` and `Matter`),
   not a `plugin:<device_id>` control screen.

### Files

`clients/couch-plugin/src/{protocol,manifest,server,host,testing}.rs` **[frozen]**,
`clients/couch-sdk/src/client.rs` **[frozen]** (`fn children(&mut self, cursor)`,
resource-taking `command/action/status`), `daemon/couch-confd/src/api/plugins.rs`
**[frozen]**, `daemon/couch-confd/src/plugins.rs` **[frozen]**,
`model/couch-model/src/connection.rs` **[frozen]** (`Provider::Plugin.children`),
`model/couch-model/src/device.rs` **[frozen]** (`child_kind`),
`model/couch-model/src/validate.rs` **[frozen]**, `model/couch-model/src/lib.rs`
**[frozen]** (`Scene.resource`), `model/couch-model/src/buttons.rs` **[frozen]**
(`function_choices` per child kind), `web/couch-web/src/screens/device_picker.rs`,
`ui/couch-gui/src/lights.rs`, `ui/couch-gui/src/activity_buttons.rs` (stop
passing an empty resource).

## (iii) A `light` component

Built-in Hue today is on/off, brightness 0 to 100, grouped rooms and scene
recall. It has no colour and no colour temperature (`docs/philips-hue.md`:
"Color editing … deferred"). Replacing it needs only the first block below;
the second is specified now so the wire shape does not have to change again.

```rust
/// What this particular lamp can do. Sent with the child, stored with the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightTraits {
    pub dimmable: bool,
    /// Colour temperature range in mirek (Hue: 153..=500), if supported.
    #[serde(default)] pub mirek: Option<(u16, u16)>,
    #[serde(default)] pub color: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LightState {
    /// None = unknown or unreachable. Never an inferred "off".
    pub on: Option<bool>,
    /// 0..=100. Kept while off: it is the level the lamp will return to.
    #[serde(default)] pub brightness: Option<u8>,
    #[serde(default)] pub mirek: Option<u16>,
    /// CIE xy in ten-thousandths, so the type stays Eq and float-free.
    #[serde(default)] pub xy: Option<(u16, u16)>,
}

pub enum TypedAction {
    SetVolumeDb { tenths: i16 },
    /// Every field optional; absent = leave alone. brightness 0 means off,
    /// as it does in built-in Hue.
    SetLight { on: Option<bool>, brightness: Option<u8>, mirek: Option<u16>, xy: Option<(u16, u16)> },
}
pub enum PluginActionSchema { SetVolumeDb { … }, SetLight }   // and lift `actions.len() > 1`
pub enum PluginComponent   { /* … */ Light, Scene }
```

`Status` gains `light: Option<LightState>`. A write answers with the state the
bridge acknowledged, so the caller needs no second round trip:
`Response::Light { state: LightState }`, which is what
`couch_hue::live::Live::{toggle, brightness}` already return.

The existing button vocabulary already covers lights (`on`, `off`, `toggle`,
`dim:N` in `model/couch-model/src/commands.rs`), so activities and key
bindings need no new functions; the host maps `dim:N` to
`SetLight { brightness: N }` for a `light` child. A scene child answers `on`
by recalling itself.

### Optimistic updates: where they live today and where they go

Two layers, and both already exist:

- **Panel** (`ui/couch-gui/src/lights.rs`): `brightness_pending` keeps one
  latest target per light (`queue_brightness`), steps are computed from the
  pending or in-flight target rather than the last observation
  (`brightness_step`), the overlay shows the target at once, at most one write
  is in flight, and sends are at least 100 ms apart (`send_brightness`). This
  is transport-agnostic and stays. It only needs a `Plugin` arm in `perform`
  that sends `Action { SetLight, resource }` over `plugin.sock`.
- **Client** (`src/live.rs` in this repository): an acknowledged write is held
  as `Pending` for 2 s so that a snapshot taken before the bridge caught up
  cannot undo it, a generation counter discards reads that raced a command,
  and unavailability stays authoritative. This moves into the package
  unchanged, because it is this crate.

One host constant matters: a queued request expires after `QUEUE_TTL` = 750 ms,
and a full resource read from the real bridge measured 0.8 to 1.1 s. A package
must therefore answer `Status` and `States` from its cache and never from a
fresh bridge read on the request path. `Live` already works this way.

Files: `model/couch-model/src/volume.rs` **[frozen]** (or a new `light.rs`
re-exported from `lib.rs` **[frozen]**), `model/couch-model/src/connection.rs`
**[frozen]**, `clients/couch-sdk/src/status.rs` **[frozen]**,
`clients/couch-plugin/src/manifest.rs` **[frozen]**, `ui/couch-gui/src/lights.rs`,
`web/couch-web/src/screens` (the existing "Show light controls" panel).

## (iv) Live state under request/response

Hue pushes changes on `GET /eventstream/clip/v2` (server-sent events). The
protocol is lockstep: the child answers, it never speaks first, and
`host.rs` relies on that for its deadlines. Two steps, the first sufficient
for Hue:

**Step 1, in v3: cheap versioned polling.** The package keeps the event stream
open on its own thread (the child is an ordinary process; `Live` already runs
a stream thread and a recovery poll thread) and serves state from memory.

```rust
Request::States  { #[serde(default)] since: Option<u64> }
Response::States { revision: u64, stale: bool, changed: Vec<ChildState>, removed: Vec<String>,
                   #[serde(default)] next: Option<u64> }
pub struct ChildState { pub id: String, pub light: Option<LightState> }
```

`since: None` returns everything (paged through `next`); otherwise only what
changed after that revision. `stale` is true while the stream is down and the
package is on its 5 s recovery poll. The panel asks every 500 ms while a
Hue-only room is open, exactly as `lights.rs` already reads `Live`'s cache; an idle answer is about 40
bytes and costs no bridge traffic.

One host change makes this work: a child with `"keep_alive": true` in its
manifest is not reaped after `IDLE` = 60 s while any device of its connection
is in a room, otherwise the cache is cold every time the panel wakes and the
first answer costs a full read. The panel's `wake` (which resets `HueFleet` today) maps to dropping the
child, so nothing observed before sleep is trusted after it.

**Step 2, later, for webOS, Kodi and Sonos: real events.** The child may send
`{"id":0,"body":{"type":"event","revision":N}}` between responses; the host's
reader demultiplexes id 0 into a per-connection channel that the daemon
offers as SSE to the browser and as a wake-up on `plugin.sock`. This touches
the deadline logic in `host.rs` and should not be bundled with the Hue work.

Files: `clients/couch-plugin/src/{protocol,server,host}.rs` **[frozen]**,
`daemon/couch-confd/src/plugins.rs` **[frozen]** (`IDLE`, `keep_alive`),
`ui/couch-gui/src/connections.rs` (`HueFleet` becomes a thin `States` cache
over `plugin.sock`), `ui/couch-gui/src/lights.rs`.

## (v) Discovery from an unprivileged child

The child runs as uid/gid 65534 with `env_clear()`, cwd `/`, `no_new_privs`
and only the `AID_INET` group. It can open ordinary sockets, so it *could*
browse mDNS itself, but Couch has already decided otherwise and for a good
reason: `clients/couch-sdk/src/discovery.rs`: "the daemon owns the mDNS
browser … so that one process, not five, holds a multicast socket on a device
with this much RAM". Keep that.

- Manifest: `"discovery": {"mdns": "_hue._tcp.local."}`. The daemon browses
  with the `mdns_sd` instance it already has (`api/streaming_tv.rs`) and hands
  the browser `[{name, address, port, txt}]`; the Hue TXT record carries
  `bridgeid` and `modelid`.
- `Request::Probe { found: Discovered }` → `Response::Probe { settings: Value,
  label: String }` lets the package turn a found address into its settings
  and a display name without pairing. This is `couch_sdk::Discover::settings_for`
  carried over the wire.
- `discovery.meethue.com` (the cloud fallback) is left out. It needs public
  web PKI roots in a static child (`webpki-roots`, a few hundred kilobytes), a working
  resolver, and it is rate-limited by Signify; mDNS plus a typed address covers
  the same ground. Built-in Hue has no discovery at all today: the address is
  typed. Discovery is an improvement, not a condition for replacement.

Files: `clients/couch-plugin/src/{protocol,manifest}.rs` **[frozen]**,
`clients/couch-sdk/src/discovery.rs` **[frozen]**,
`daemon/couch-confd/src/api/streaming_tv.rs` (generalise the browse),
`daemon/couch-confd/src/api/plugins.rs` **[frozen]**,
`web/couch-web/src/screens/connections.rs`.

## (vi) The bridge certificate inside a package

Facts: the bridge serves HTTPS only. Its certificate names the bridge id, not
its address, so hostname validation against an IP always fails. The real
bridge read during this work presents a leaf with subject
`C=NL, O=Philips Hue, CN=<bridge id>` issued by `C=NL, O=Philips Hue,
CN=root-bridge`, Signify's private CA; older firmware presents a self-signed
certificate with the same subject.

What built-in does, and what this crate does: trust on first use during the
link-button pairing, store the exact leaf DER, and refuse any other
certificate afterwards (`couch_sdk::tls::Pin`, used through `src/tls.rs`).
Handshake signatures are still verified; only chain and name checks are
replaced by the exact match.

In a package this works unchanged **once (i) exists**: the DER goes into the
credential as base64 (578 bytes on the real bridge; well under the 16 KiB
cap), and `Configure` hands it back on every start. Without credential
write-back there is nowhere to keep it, which is reason enough on its own not
to ship a protocol 2 package.

A worthwhile tightening, possible because pairing becomes a package concern:
during pairing only, additionally require that the leaf either chains to the
Signify `root-bridge` CA (its PEM is published in the Hue developer
documentation and would be embedded in this crate) with `CN` equal to the
bridge id reported by `GET /api/0/config` or by the mDNS TXT record, or is
self-signed with that `CN`. That closes the first-use window against anyone
who is not the bridge, and costs nothing at run time because the pin still
does the work afterwards.

Build note: TLS means `rustls` with the `ring` provider, which compiles C and
assembly. The feed builds packages after sourcing Couch's `tools/arm-cc-env.sh`
(Zig as the ARM musl C compiler, `rust-lld` as linker), so this crate
cross-builds there as it is; this repository's CI proves it on every push.
Nothing here links OpenSSL, `native-tls` or `aws-lc`.

## Order of work

1. (ii) resource addressing and `Children`, with (iii) `light`/`scene`
   components: smallest change that makes a packaged light look like a light.
2. (i) pairing and credential write-back: unblocks Hue and all four TVs.
3. (iv) step 1, `States` and `keep_alive`.
4. The package adapter in this repository, the admission cases against a fake
   bridge, a preview release through the feed, and a side-by-side run against
   built-in Hue on a real bridge.
5. Automatic conversion of an old Hue connection, as was done for Denon:
   credential file → package credential, `room:`/`scene:` → `room/`/`scene/`,
   `Scene.hue` → `Scene.resource`. Only then is built-in Hue removed.
6. (v) discovery and (iv) step 2 whenever convenient; neither blocks removal.
