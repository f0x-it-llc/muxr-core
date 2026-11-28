# Muxr client API (`muxr.v1`)

The gRPC contract muxrd serves: package `muxr.v1`, service `Muxr`, one file
per domain under `muxr/v1/`, with this directory (`muxrd/proto/`) as the
include root. Imports are written `muxr/v1/<file>.proto`. The comments in
each file state what a single RPC, message or field means; this page states
the rules that need more than a line and the ones several RPCs share.

| File | Contents |
|---|---|
| `muxr/v1/muxr.proto` | `service Muxr`: all RPCs, grouped by domain |
| `muxr/v1/common.proto` | `Empty`, `Backend`, `ActionAck`, `SessionRef`, `PaneTarget`, `TabTarget` |
| `muxr/v1/version.proto` | `BackendVersion`, `VersionInfo` |
| `muxr/v1/auth.proto` | `LoginRequest`, `LoginResponse`, `TokenInfo`, `TokenList`, `CreateTokenReq`, `RevokeTokenReq` |
| `muxr/v1/push.proto` | `RegisterPushTargetReq`, `ListPushTargetsReq`, `PushTargetInfo`, `ListPushTargetsResp`, `RemovePushTargetReq` |
| `muxr/v1/terminal.proto` | `ClientFrame`, `ServerFrame`, `AttachReq`, `Resize`, `MouseKind`, `MouseInput`, `ControlEvent` |
| `muxr/v1/session.proto` | `SessionList`, `SessionInfo`, `RenameSessionReq`, `CreateSessionReq` |
| `muxr/v1/layout.proto` | `Layout`, `TabMsg`, `PaneMsg` |
| `muxr/v1/pane.proto` | `WriteToPaneReq`, `NewPaneReq`, `RenamePaneReq`, `ResizeDirection`, `ResizeKind`, `ResizePaneReq`, `ToggleFullscreenReq`, `ScrollDirection`, `ScrollReq` |
| `muxr/v1/tab.proto` | `NewTabReq`, `RenameTabReq` |
| `muxr/v1/space.proto` | `Space`, `SpaceList`, `SwitchSpaceReq`, `CreateSpaceReq`, `RenameSpaceReq`, `CloseSpaceReq` |
| `muxr/v1/agent.proto` | `AgentStatus`, `AgentMark`, `AgentMsg`, `AgentSummary`, `AgentList`, `AgentEvent` |
| `muxr/v1/monitor.proto` | `VitalsDetail`, `WatchVitalsRequest`, `VitalsEvent`, `HostInfo`, `LoadAverage`, `HostMemory`, `DiskVolume`, `NetInterface`, `HostTemperature`, `VitalsSample`, `HistoryRange`, `VitalsHistoryRequest`, `VitalsHistory`, `SeriesKind`, `HistorySeries`, `ProcessSort`, `WatchProcessesRequest`, `ProcessState`, `ProcessRow`, `ProcessSnapshot`, `ProcessSignal`, `SignalProcessRequest` |

## Compiling

muxrd's `build.rs` collects every `.proto` below this directory (in sorted
order, skipping names that start with `.`) and compiles them with
`tonic-prost-build`, include root `proto`. The generated module is
`muxr.v1`, included by `src/lib.rs`. The build also writes a
`FileDescriptorSet` that the wire-surface test reads.

A client compiles all thirteen files with this directory as the include root,
for example:

```sh
protoc -I muxrd/proto --descriptor_set_out=/dev/null muxrd/proto/muxr/v1/*.proto
grpcurl -import-path muxrd/proto -proto muxr/v1/muxr.proto <host>:<port> muxr.v1.Muxr/GetVersion
```

## Compatibility

The wire is frozen by `muxrd/tests/wire_surface.golden`: a canonical listing
of every RPC, message field and enum value of `muxr.v1`, rendered from the
descriptor set and compared by `cargo test -p muxrd --test wire_surface`. A
change to that listing is a wire change. Moving a definition between files,
or editing a comment, never changes it. When a wire change is intended,
review it and regenerate the golden:

```sh
UPDATE_WIRE_GOLDEN=1 cargo test -p muxrd --test wire_surface
```

`buf.yaml` sets the `PACKAGE` breaking-change rules. It has no lint block:
the existing names (the `Req` suffixes, enums whose zero value is not
`*_UNSPECIFIED`) predate buf's `STANDARD` rules and are not renamed, because
renaming them would break every generated client.

## Authentication

`GetVersion` and `Login` need no authentication. Every other RPC needs
`authorization: Bearer <session_token>`, the token `Login` returns.

`Login` takes a persistent auth token, the kind zellij's
`web_authentication_tokens` creates (and that `CreateToken` creates).
`remember_me` selects a session token that lives 28 days instead of about
five minutes. An auth token is either read-write or read-only, and the
session token inherits that tier; `LoginResponse.is_read_only` reports it so
a client can disable the controls the server would refuse.

A **read-only** token may:

- call every read: `ListSessions`, `GetLayout`, `GetSpaces`, `ListAgents`,
  `WatchAgents`, `WatchVitals`, `GetVitalsHistory`, `WatchProcesses` (command
  lines empty, see [Host vitals](#host-vitals));
- navigate its own view: `GoToTab`, `FocusPane`, `ScrollPane`,
  `SwitchSpace`;
- `AttachTerminal`, resume hints included (see [Resume hints](#resume-hints)).

Everything that mutates session content or shared layout is refused with
`PERMISSION_DENIED`: `WriteToPane`, `ClosePane`, `NewPane`, `RenamePane`,
`ResizePane`, `TogglePaneFloating`, `TogglePaneFullscreen`, `NewTab`,
`CloseTab`, `RenameTab`, `RenameSession`, `KillSession`, `CreateSession`,
`CreateSpace`, `RenameSpace`, `CloseSpace`, `SignalProcess`. `ResizePane` is refused even
though it looks like navigation: a resize has no per-connection routing and
changes the layout every client sees.

Token management (`CreateToken`, `ListTokens`, `RevokeToken`) and the push
device registry (`RegisterPushTarget`, `ListPushTargets`,
`RemovePushTarget`) are admin-gated: a read-only token is refused with
`PERMISSION_DENIED`. Token management wraps the same zellij
`web_authentication_tokens` operations the zellij CLI uses. The token secret
leaves the server once, in the `CreateToken` answer; `ListTokens` returns
metadata only. Device names and handle prefixes are sensitive fleet
metadata, and registration and removal mutate, which is why the registry is
admin-gated.

A push handle is an opaque capability issued by the notification relay. No
RPC ever returns it in full; `ListPushTargets` gives only its first 8 hex
characters, enough to tell devices apart in a UI and never enough to
reconstruct the handle. Registering a `device_name` that already exists
replaces that entry, which is how a client refreshes its handle.

## Capabilities

`VersionInfo.capabilities` lists additive feature flags. A client must
ignore values it does not know.

| Flag | Meaning when present | When absent |
|---|---|---|
| `mouse-input` | `AttachTerminal` understands `ClientFrame.mouse`. | Do not send mouse frames. |
| `push-notifications` | The server has a notification relay configured; `notification_relay_url` names it. | Push is unsupported. |
| `agents` | At least one served backend reports agents: `ListAgents` can return entries, and `PaneMsg.agent` and `SessionInfo.agents` may be set. | `ListAgents` answers empty and those fields stay unset. |
| `agents-stream` | `WatchAgents` delivers live agent changes. | `WatchAgents` sends its one snapshot and ends; poll `ListAgents` instead. |
| `host-vitals` | Host monitoring is on: `WatchVitals`, `GetVitalsHistory`, `WatchProcesses` and `SignalProcess` are served. Read at call time, so it can appear or vanish between calls. | Do not call those four RPCs; they answer `FAILED_PRECONDITION`. |

`VersionInfo.notification_relay_url` is the relay clients register devices
with. Empty means the server has no relay configured, and the client must
treat push notifications as unsupported. `RegisterPushTarget` fails with
`FAILED_PRECONDITION` on such a server.

`VersionInfo.latest_version` is the newest released muxrd version, as plain
`MAJOR.MINOR.PATCH`, and is set only when that release is newer than
`server_version`. The server does the comparison; a client shows the value
and compares nothing. Empty means there is nothing to report: the server is
current, has not completed a check yet, or has its daily check disabled. The
field is additive and has no capability flag: an older client ignores it, and
an older server leaves it empty.

## Attach

`AttachTerminal` is one bidirectional stream per terminal view: the client
sends `ClientFrame`s, the server sends `ServerFrame`s.

- The **first** `ClientFrame` of every stream must be `attach`
  (`AttachReq`): the session and the initial `rows` and `cols`. After it, the
  client sends `input` (raw bytes for the focused pane), `resize` and, with
  the `mouse-input` capability, `mouse`.
- The server sends `render` (raw ANSI bytes of the session) and `control`
  (`ControlEvent`, an out-of-band `kind` and `payload`). The first frame the
  server sends is a `control` event of kind `connection_id`; see
  [Routing](#routing).

A herdr stream shows one pane. A zellij stream is the whole composited
session and follows the zellij server's own focus.

### Mouse input

`MouseInput` coordinates are **zero-based cells** in this stream's own grid:
the grid that `AttachReq` and `Resize` `rows` and `cols` established, and
the one `GetLayout` reports geometry in. The multiplexer resolves the pane
under the position, then either forwards the event to the pane's
application (when that application captured the mouse) or scrolls the pane
itself. Only the wheel kinds exist; press and drag would be added as new
`MouseKind` values.

### Resume hints

A reconnecting client may name the space, tab and pane it was last viewing,
so the fresh attach lands there instead of on the backend's daemon-global
focus:

- `resume_space_id`: the opaque herdr workspace id `GetSpaces` and
  `SwitchSpace` use. Empty = the daemon's active-or-first workspace. This is
  the hint that survives a server restart.
- `resume_tab_id`: `GetLayout`'s `TabMsg.tab_id`. 0 = no tab hint. It needs
  no corroboration.
- `resume_pane_id`: `GetLayout`'s `PaneMsg.id`. 0 = no pane hint. It is
  honoured **only** when `resume_tab_id` is also set and the pane sits in
  that tab; a pane hint without a tab hint is ignored even when it would
  resolve. Send tab and pane together.

The hints are best-effort. Resolution falls through pane, then tab, then
space on anything stale, unknown or uncorroborated, and an unresolvable
hint **never** fails the attach: it degrades to the next hint, and with no
usable hint the attach shows the session's currently focused pane, exactly
as when no hint is sent.

Hints are honoured for every attach, read-only included, with the same
resolution and fall-through. This stays inside the read-only boundary in
the same way `FocusPane`, `GoToTab` and `SwitchSpace` do: it is navigation
within the viewer's own stream and moves nothing for anybody else, and it
discloses nothing new, because a read-only token can already read every
space's tab and pane structure through `GetSpaces` and `GetLayout`.

Hints apply to herdr sessions only. A zellij attach ignores them.

### Client instance id

`AttachReq.client_instance_id` is an opaque id the **client** chooses for
one client view, for example one terminal screen in the app, and keeps
stable across every re-attach of that view (a network change, a resume
from background). It must be unique per view, for example a UUID minted
once per mounted view. The server never interprets it; it only compares it
for equality.

- When an attach arrives with a non-empty id and an earlier attach that is
  still live carries the **same** id under the **same** authenticated bearer
  token, the earlier attach is torn down (its stream ends and its backend
  client detaches) and the new one takes its place. A stale connection the
  server cannot yet see as dead therefore does not linger as an extra
  client. The new attach never waits for that teardown.
- Eviction is scoped to the authenticated token: the same id under a
  different token never touches another token's attach, and two different
  ids under one token never evict each other. Read-only and read-write
  tokens behave the same.
- Empty = no eviction either way: the attach neither evicts nor can be
  evicted.
- An id longer than 128 bytes is ignored (treated as empty), never rejected.
- Order is the server's receipt of the attach request: the later-received
  attach wins, even if its predecessor's setup finishes later.

## Space-scoped layout reads

`GetLayout` reads `SessionRef.space_id` (every other RPC taking a
`SessionRef` ignores it):

- Empty = the space the relay focused for this connection, or else the
  backend's active-or-first workspace.
- Non-empty = an opaque herdr space id. The answer is **that** space's tabs
  and panes, and the read never moves this connection's focus or the
  daemon's. On a spaces backend the id addresses the space directly:
  `session` neither narrows nor disambiguates it.
- Naming the connection's **own** current space answers with the same
  per-connection `active` and `is_focused` view the empty path gives; a
  foreign space answers with the backend's raw values.
- An id naming no live space answers `NOT_FOUND`, distinct from a real
  space that happens to be empty. Every other backend failure is a terse
  `INTERNAL` that never carries the daemon's error chain.
- A backend without spaces (zellij) rejects a non-empty value with
  `INVALID_ARGUMENT`.

**Read-only trade-off (deliberate).** `GetLayout` is a read, so, like
`GetSpaces`, it is not gated by the read-only check. A read-only token can
therefore enumerate the daemon's spaces with `GetSpaces` and read the tab
and pane **structure** of every one of them through `space_id`, not just the
space its own connection is viewing; the `NOT_FOUND` answer also tells it
which ids exist. Pane **content** still needs an attach, and nothing on
this path mutates or moves focus. `GetLayout` and `GetSpaces` are one
disclosure surface and must agree: do not add a read-only rejection to one
without revisiting the other. `ListAgents` and `WatchAgents` belong to the
same surface (agent kinds, names, titles and summaries are structure, not
pane content).

`SessionInfo.space_count` gives the number of spaces per session in the
`ListSessions` answer, so a client can show it without calling `GetSpaces`
per session.

## Routing

Several clients can attach to one session, each viewing a different pane,
tab or space. Per-connection routing makes an RPC act on the view of the
stream that sent it rather than on the backend's shared focus.

`AttachTerminal` mints a connection id for every stream and sends it as the
stream's first server frame, a `ControlEvent` with `kind` `connection_id`
and the id as `payload`. The client echoes it in the `connection_id` field
of `SessionRef`, `PaneTarget`, `TabTarget`, `NewPaneReq`, `NewTabReq`,
`SwitchSpaceReq`, `CreateSpaceReq`, `RenameSpaceReq` and `CloseSpaceReq`, so
the RPC is routed to that stream's relay and not to another relay on the
same session. Empty = legacy routing, scoped to the session.

### Routed create

`NewPane`, `NewTab` and `CreateSpace` share one contract when
`connection_id` names a live connection on herdr. The new object is created
without moving herdr's focus, and that connection's stream is re-pointed at
it: `NewPane` splits the connection's attached pane, `NewTab` creates the
tab in the connection's space, `CreateSpace` switches the connection to the
new space. A non-empty id naming no live connection is refused ("reattach
required").

The answer:

| `ok` | `info` | `error` | Meaning |
|---|---|---|---|
| true | new id | | Created, and this connection now shows it. |
| false | new id (same format) | starts with `created-not-attached:` | Created, but this connection could not be re-pointed at it. Do not create it again; reattach to reach it. |
| false | empty | starts with `create-outcome-unknown:` | The server cannot tell whether it was created (e.g. no answer in time). Re-read the layout before retrying. |
| false | | anything else | Nothing was created. |

The new id is `terminal_<id>` for a pane, the new tab id for a tab, and the
new space id for a space. On zellij, which has no per-connection create, the
id is not used for routing and the request takes the legacy path.

## Fullscreen hint

To toggle fullscreen the server needs three facts about the target pane:
whether it is floating, whether floating panes are visible in its tab, and
whether it is the focused floating pane. A client that already polls them
(`PaneMsg.is_floating`, `TabMsg.floating_panes_visible`,
`PaneMsg.is_focused`) sends them in `ToggleFullscreenReq`, which spares the
server a synchronous backend query on the interaction path.

`has_floating_hint` is the presence flag. proto3 bools default to false, so
an all-false hint cannot be told apart from no hint. A caller that knows the
floating state must set `has_floating_hint = true`, and the server then
trusts the three bools. With `has_floating_hint = false` (e.g. a target-only
request) the server ignores them and queries the live state, so a caller
without a hint can never silently send a floating pane down the tiled path.
`target_is_focused_floating` is true only when the target is the focused,
visible floating pane (`target_is_floating && floating_visible &&
is_focused`).

## Closing a space

On herdr a space can be the **primary** of a worktree group: the workspaces
for its linked git worktrees share its repository key, and herdr closes the
whole group as one unit.

`CloseSpaceReq.close_group` is opt-in and defaults to false, so a client
that does not know the field gets the safe behaviour:

- `false`: close only the named space. herdr refuses to close a group
  primary, and muxrd answers `ActionAck{ok: false}` naming the group and how
  to proceed. That refusal is the intended outcome, not an error to route
  around: it is how the operator learns that closing one space would have
  destroyed several.
- `true`: remove the primary and every linked-worktree space with it. The
  ack's `info` names each space removed, so the client can reconcile its
  space list. muxrd still refuses a group close that would leave the daemon
  with no spaces.

## Agents

An agent is an interactive coding-assistant CLI the backend has recognised
in a pane. Only backends that report agents (capability `agents`) populate
the agent fields; every other backend answers an empty `AgentList`.
`PaneMsg.agent` is unset when its pane runs no agent. `SessionInfo.agents`
is unset when the backend reports no agents, the session is resurrectable,
or the agent query failed or timed out; that never drops the session from
the list.

### ListAgents

`ListAgents` returns every agent in the session, across all of its spaces,
not just the space this connection is viewing. Only `SessionRef.session` is
read; `connection_id` and `space_id` are ignored. It is not relay-routed:
it reads the backend directly and moves no focus. It is a read, permitted
for read-only tokens.

Order is most urgent first: blocked, then done, working, idle, unknown;
within one status, the newest state change first (larger
`AgentMsg.change_seq`). `AgentList.summary` counts the agents and is always
set, even when the list is empty; `AgentSummary.kinds` lists distinct kinds
in that same order, at most 8.

A backend without an agent concept (e.g. zellij) answers an empty
`AgentList`, never an error. Errors:

- an unknown backend token in the session id, or a rejected session name:
  `INVALID_ARGUMENT`;
- a known backend this server is not driving: `NOT_FOUND`;
- a session name the backend does not know: `NOT_FOUND`;
- a backend failure or timeout: `INTERNAL`.

### WatchAgents

`WatchAgents` watches the same session-wide inventory. It is a read,
permitted for read-only tokens, not relay-routed, reads only
`SessionRef.session`, and is advertised by the `agents-stream` capability.

- The stream opens with one `snapshot`: exactly what `ListAgents` would
  answer.
- Then deltas: a `changed` per agent whose state moved, carrying the
  agent's whole current `AgentMsg` (not a diff; insert or replace by
  `pane_id`), and a `removed_pane_id` when an agent's pane closes or its
  agent leaves the inventory.
- A transition may arrive as two `changed` messages: the status first, then
  the reconciled row once the server's next backend re-read completes
  (re-reads are at least 250 ms apart). The later one is authoritative.
- A server that fell behind its own event feed, or whose feed reconnected to
  the backend, sends a fresh `snapshot`, which replaces everything the
  client holds.
- A backend without an agent concept sends the empty snapshot and ends the
  stream. A server with no live agent event feed sends its one snapshot and
  ends; poll `ListAgents` when `agents-stream` is absent.

Errors are `ListAgents`' own, and come before the first message. After it, a
failed backend re-read never ends the stream: the server keeps what it
already sent and retries in the next window. The session token is
re-validated every 30 s, even while the client is not reading, and a
revoked token ends the stream with `UNAUTHENTICATED`. Otherwise the stream
ends only when the client goes away or the server stops.

### Completion counter

`AgentMsg.completion_seq` is a monotonic counter of the agent's last
completion. proto3 cannot tell 0 from unset, and an agent that has never
completed has no completion counter, so `has_completion_seq` is its
presence flag: read `completion_seq` only when `has_completion_seq` is true.

## Host vitals

`WatchVitals`, `GetVitalsHistory`, `WatchProcesses` and `SignalProcess`
(`muxr/v1/monitor.proto`) report on the machine muxrd runs on, and are gated
by the `host-vitals` capability: a client must not call them unless
`GetVersion` lists it, and each answers `FAILED_PRECONDITION` when
monitoring is off at call time; a stream open when monitoring is switched off
sends one message with `disabled = true` and ends with OK. Every `revision`
increases monotonically for the life of the server process. The two watches
and `GetVitalsHistory` are reads, permitted for read-only tokens;
`SignalProcess` is refused for them with `PERMISSION_DENIED`. A process
command line is never shown to a read-only token, and a full token sees it
only for processes owned by the user muxrd runs as; every other row carries
an empty `command`. Read-only tokens still see every process's pid, name,
user and numbers.

Each stream is paced by its own interval (`WatchVitals` summary 5 s, full
`interval_ms` clamped to 2-10 s, 0 = 3 s; `WatchProcesses` `interval_ms`
clamped to 3-10 s, 0 = 3 s): it gets at most one message per interval,
whatever other clients ask of the server. At most 16 `WatchVitals` and 16
`WatchProcesses` streams are open server-wide; the next open answers
`RESOURCE_EXHAUSTED`. After a lag, `VitalsEvent.lagged` counts the skipped
samples and the event carries the next sample the server still had, which may
itself be older than the newest. `WatchProcesses` `limit` 0 = 50 and above
200 = 200; `pinned_start_unix_s` is ignored when `pinned_pid` is 0, and a
pinned row is matched by pid and start time. A fresh daemon's history is
mostly NaN until the rings fill.

`SignalProcess` answers `PERMISSION_DENIED` for a read-only token,
`FAILED_PRECONDITION` when monitoring is off, and `INVALID_ARGUMENT` for pid 0
or an unspecified or unknown signal. A target that is not found, has changed since it was
listed (start time), is not allowed (pid 1, muxrd itself, a thread id) or
is refused by the operating system comes back as `ActionAck{ok: false}` with
display-only error text: a client treats any `ok: false` as "not signalled"
and must not parse the text. `ok: true` means the signal was sent, not that
the process exited. A full token may signal any process the user muxrd runs as
may signal.
