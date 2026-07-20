# Design: `msquic_async::Registration` and `wait_idle()`

Status: implemented
Target crate: `msquic-async` (0.4.1 → 0.5.0, breaking)
Prior art: [`msquic-h3` commit `b4be2599`](https://github.com/youyuanwu/msquic-h3/commit/b4be2599d1b18710d79dec314817555efaa73668)

Revision history:

- Streams are now tracked. §9 originally claimed a `Stream` outliving its
  `Connection` was an unrelated, pre-existing hazard; that was wrong on both
  counts, and it was a hole in the `wait_idle()` guarantee. See §9.

## 1. Problem

Dropping a `msquic::Registration` blocks — potentially forever — unless every
child handle derived from it has already been closed.

The upstream binding's `Drop` calls `RegistrationClose` unconditionally
(`msquic/src/rs/lib.rs:807-811`):

```rust
fn close_inner(&self) {
    if !self.handle.is_null() {
        unsafe { Api::ffi_ref().RegistrationClose.unwrap()(self.handle) }
    }
}
```

In the core library that lands in `CxPlatRundownReleaseAndWait`
(`msquic/src/core/registration.c:53`), a *synchronous, uninterruptible* wait on
a rundown reference that every child object holds:

| Child | Acquire | Release |
| --- | --- | --- |
| Connection | `connection.c:524` (`QuicConnRegister`) | `connection.c:411`, `:502`, `:547` (via `ConnectionClose`) |
| Listener | `listener.c:93` (`QuicListenerOpen`) | `listener.c:104`, `:167` (via `ListenerClose`) |
| Configuration | `configuration.c:204` (`QuicConfigurationOpen`) | `configuration.c:271`, only at refcount 0 |

Two properties make this hard to get right from application code:

1. **`RegistrationShutdown` closes nothing.** `Registration::shutdown()` queues
   shutdown on connections and stops listeners, but the handles — and their
   rundown references — stay alive until the Rust values are dropped.
2. **`SHUTDOWN_COMPLETE` is not close.** `msquic-async` sets
   `ConnectionState::ShutdownComplete` in `handle_event_shutdown_complete()`
   (`msquic-async/src/connection.rs:848`) and `ListenerState::ShutdownComplete`
   in `handle_event_stop_complete()` (`listener.rs:309`), but neither handler
   closes a handle. Any application-level "wait for shutdown" built on these
   events fires *before* the rundown reference is released, and is therefore
   racy.

So today the only correct teardown rule is an unwritten one: *drop every
`Listener`, every clone of every `Connection`, and every `Configuration` before
the `Registration` goes out of scope.* Nothing in the crate enforces it, nothing
detects a violation, and the failure mode is a silent hang inside `Drop`.

This is aggravated by `Connection` being `Clone` (`Connection(Arc<ConnectionInstance>)`,
`connection.rs:27`). `h3-msquic-async` clones it into four boxed unfold futures
plus `OpenStreams` (`h3-msquic-async/src/lib.rs:44-58`, `:221-228`), so "the
application dropped its `Connection`" says nothing about whether
`ConnectionClose` has run.

## 2. Goals / Non-goals

**Goals**

- Provide `Registration::wait_idle()`, an async signal that resolves once it is
  safe to drop the `Registration` — i.e. once every `msquic-async` handle that
  `RegistrationClose` could invalidate has been closed.
- Make it structurally impossible to create an untracked connection, listener or
  stream, by routing every construction path through the wrapper.
- Keep the tracking free of dependencies beyond `std`, usable from any executor,
  and correct under an arbitrary number of concurrent waiters.

Note the goal is deliberately *not* "resolve once the native rundown has
drained". Those two are not the same, and §9 is exactly where they diverge.

**Non-goals**

- Tracking `Configuration` handles. See §7 — a configuration guard would report
  idle *too early*, which is worse than not tracking at all.
- Making `Registration::drop` non-blocking, or adopting `RegistrationClose2`.
  The FFI entry exists (`msquic/src/rs/ffi/linux_bindings.rs:6726`) but is
  unused by the binding; wrapping it is a separate change.

## 3. Design overview

A new private module `msquic-async/src/registration.rs` introduces four types,
mirroring the `msquic-h3` design:

- **`RundownState`** — `AtomicUsize` count of live reservations plus a
  `Mutex<Waiters>` waker table. Owned by the `Registration`, cloned via `Arc`
  into every tracked handle.
- **`RundownGuard`** — RAII token. Increments on construction, decrements on
  drop, and wakes all waiters on the `1 → 0` edge.
- **`Registration`** — public wrapper owning `msquic::Registration` plus
  `Arc<RundownState>`. The raw handle is *not* public.
- **`WaitIdle`** — the `Future` returned by `wait_idle()`, supporting any number
  of concurrent waiters via a keyed waker map.

The counter is a conservative over-approximation of the native rundown: we
reserve *before* opening a handle and release *after* closing it, so `active == 0`
implies the native rundown holds no `msquic-async` object, never the reverse.

The implementation of these four types can be taken essentially verbatim from
`msquic-h3/src/registration.rs`; the interesting work in this crate is §5–§7.

## 4. Public API

```rust
// msquic-async/src/lib.rs
mod registration;
pub use registration::{Registration, WaitIdle};
```

```rust
impl Registration {
    /// Open a registration and its rundown tracking state.
    pub fn new(config: &msquic::RegistrationConfig) -> Result<Self, msquic::Status>;

    /// Queue shutdown on all connections and stop all listeners. Non-blocking,
    /// and closes nothing — pair it with `wait_idle()`.
    pub fn shutdown(&self);

    /// Open a `Configuration` against this registration.
    ///
    /// Required because the raw registration handle is not public. The returned
    /// `Configuration` is deliberately untracked: drop it *after* `wait_idle()`
    /// resolves and *before* dropping the `Registration`.
    pub fn open_configuration(
        &self,
        alpn: &[msquic::BufferRef],
        settings: Option<&msquic::Settings>,
    ) -> Result<msquic::Configuration, msquic::Status>;

    /// Resolves once no `msquic-async` `Connection` or `Listener` holds this
    /// registration's rundown.
    pub fn wait_idle(&self) -> WaitIdle;

    pub(crate) fn raw(&self) -> &msquic::Registration;
    pub(crate) fn state(&self) -> &Arc<RundownState>;
}

impl Future for WaitIdle { type Output = (); }
```

Changed constructors:

| Before | After |
| --- | --- |
| `Connection::new(&msquic::Registration)` | `Connection::new(&crate::Registration)` |
| `Listener::new(&msquic::Registration, msquic::Configuration)` | `Listener::new(&crate::Registration, msquic::Configuration)` |
| `msquic::Configuration::open(&registration, alpn, settings)` | `registration.open_configuration(alpn, settings)` |

**Additionally proposed for this crate** (not present in the prior art): a
diagnostic `Drop` on `Registration` that logs before the blocking close, so the
hang this design prevents is at least identifiable when it is reintroduced:

```rust
impl Drop for Registration {
    fn drop(&mut self) {
        let active = self.state.active.load(Ordering::Acquire);
        if active != 0 {
            tracing::warn!(
                active,
                "Registration dropped with live handles; RegistrationClose will block. \
                 Call shutdown() then wait_idle().await before dropping."
            );
        }
    }
}
```

`tracing` is already a dependency, so this costs nothing.

## 5. Guard placement — the load-bearing part

A guard must decrement **after** the `*Close` it protects, and **only when the
last owner of the handle is gone**. Field drop order in Rust is declaration
order, so the guard is declared *after* the handle field.

### 5.1 `Connection`

`Connection` is `Arc<ConnectionInstance>` and `Clone`. The guard therefore
belongs in `ConnectionInstance`, not in `Connection`:

```rust
struct ConnectionInstance {
    inner: Arc<ConnectionInner>,
    msquic_conn: msquic::Connection,
    // Declared last: `msquic_conn`'s ConnectionClose releases the native
    // rundown ref before this guard decrements and wakes wait_idle waiters.
    _guard: RundownGuard,
}
```

This is exactly the correction the prior art calls out: binding the decrement to
a user-facing wrapper's `Drop` is wrong when the handle lives behind a shared
`Arc`. Here the last `Arc<ConnectionInstance>` clone — including the ones inside
`h3-msquic-async`'s boxed futures — triggers `ConnectionInstance::drop`, which
drops `msquic_conn` (→ `ConnectionClose`, `msquic/src/rs/lib.rs:953`) and only
then the guard.

Note the callback closure captures `Arc<ConnectionInner>`, never
`ConnectionInstance` (`connection.rs:36-38`), so there is no reference cycle
keeping the guard alive.

### 5.2 `Listener`

```rust
pub struct Listener {
    inner: Arc<ListenerInner>,
    msquic_listener: msquic::Listener,
    _guard: RundownGuard,
}
```

`Listener` is not `Clone`, so its own `Drop` is the last owner. Dropping
`msquic_listener` runs `ListenerClose` (`msquic/src/rs/lib.rs:1133`), which
drops the boxed callback closure → last `Arc<ListenerInner>` →
`ListenerInnerShared.configuration` → `ConfigurationClose`, and drops any queued
but never-accepted `Connection`s in `ListenerInnerExclusive.new_connections`.
All of that happens before the guard decrements. See §7 for why this makes the
server-side configuration safe for free.

### 5.3 Inbound connections

`ListenerInner::handle_event_new_connection` (`listener.rs:234`) constructs a
`Connection` via `Connection::from_raw`, so it needs access to the state. Add it
to `ListenerInnerShared`:

```rust
struct ListenerInnerShared {
    configuration: msquic::Configuration,
    rundown: Arc<RundownState>,
}
```

and reserve immediately before the handle is handed to a `Connection`:

```rust
connection.set_configuration(&self.shared.configuration)?;
let guard = RundownGuard::new(self.shared.rundown.clone());
let new_conn = Connection::from_raw(connection, tls_secrets, sslkeylog_file, guard);
```

Reserving *here* rather than at the top of the callback is deliberate. There is
no untracked window to close: this callback only runs while the `Listener` is
alive, and the listener's own guard keeps `active >= 1` for its whole duration.
Reserving at the top would instead introduce a drop-order bug on the `?` paths —
`connection` is a function parameter and a top-of-function `guard` would be a
local, and locals drop *before* parameters, so the guard would decrement before
`ConnectionClose`. Reserving after the last `?` makes every early-return path
trivially leak-free.

### 5.4 Streams

```rust
pub(crate) struct StreamInstance {
    inner: Arc<StreamInner>,
    msquic_stream: msquic::Stream,
    _guard: RundownGuard,
}
```

`Stream`, `ReadStream` and `WriteStream` all wrap `Arc<StreamInstance>`, so one
guard covers every alias of a stream. See §9 for why streams are tracked at all.

Streams are created from two places, and neither has a `Registration` in hand:

- `Stream::open` from `OpenOutboundStream::poll` (`connection.rs`), which has
  `&ConnectionInstance`.
- `Stream::from_raw` from `handle_event_peer_stream_started`, a method on
  `ConnectionInner` — the *callback context*, which does not hold the
  `ConnectionInstance`.

So `Arc<RundownState>` is stored on `ConnectionInner` (not only on
`ConnectionInstance`), and both sites reserve with
`RundownGuard::new(rundown.clone())`. `RundownGuard` exposes `state()` so
`Connection::from_raw`, which receives a guard rather than a `Registration`, can
seed `ConnectionInner` from it.

No cycle is created: `RundownState` is owned by the `Registration` and refers to
nothing. This is why the guard is *not* implemented by having `StreamInstance`
hold an `Arc<ConnectionInstance>` — `ConnectionInnerExclusive.inbound_streams`
holds `Stream` values, so that would make unaccepted inbound streams into a
reference cycle and leak the connection.

`Connection::from_raw` gains a `guard: RundownGuard` parameter. It is
`pub(crate)`, so this is not a public API change.

## 6. Reservation ordering rules

The rule is: **reserve before the native acquire, release after the native
release.**

- **`Listener::new`** — reserve before `msquic::Listener::open`, because
  `QuicListenerOpen` acquires the rundown before returning (`listener.c:93`). If
  `open` fails, the local guard drops and releases.
- **`Connection::new`** — reserve before `msquic::Connection::open`. Note that
  `QuicConnRegister` (`connection.c:524`) actually runs at `open` time for
  client connections, so reserving first closes the window.
- **Inbound** — reserve after the last fallible step of the `NewConnection`
  callback; see §5.3 for why that is both sufficient and safer.

`msquic-async` is simpler than the prior art here in one respect. `msquic-h3`
had to convert `Connection::connect` from `async fn` to a non-`async` function
returning `impl Future`, so that the reservation happened when the future was
*created* rather than when first polled — otherwise a queued-but-unpolled
connect could open a native handle after `wait_idle` had already observed zero.
This crate has no such function: `Connection::new` is already synchronous and
`start()` operates on an already-open, already-tracked handle. **No signature
needs to be de-`async`ed.**

## 7. Why configurations stay untracked

`ConfigurationClose` only drops the *application's* reference. Each connection
takes its own via `QuicConfigurationAddRef`, and the rundown reference is
released at `configuration.c:271` only when the refcount reaches zero. A guard
attached to a `Configuration` wrapper would decrement at `ConfigurationClose`
while the core object — and its rundown reference — is still alive, so
`wait_idle()` could resolve while `RegistrationClose` would still block. That is
strictly worse than the current situation, because it converts a documented
ordering requirement into a silent lie.

The contract therefore keeps one manual step: **drop configurations after
`wait_idle()` resolves.**

There is a pleasant consequence specific to this crate, though. `Listener::new`
takes `configuration` **by value** and stores it in `ListenerInnerShared`
(`listener.rs:204-206`). So for the server side:

1. The listener's guard decrements only after `ListenerClose` → callback closure
   dropped → `ListenerInner` dropped → `ConfigurationClose`.
2. Each connection using that configuration releases its own config ref during
   `ConnectionClose`, i.e. before its own guard decrements.

Therefore when `active` reaches 0, the server configuration is fully released.
**Only the client-side, application-owned `Configuration` passed to
`Connection::start()` requires manual ordering.**

## 8. Teardown contract

Client:

```rust
let reg = msquic_async::Registration::new(&msquic::RegistrationConfig::default())?;
let config = reg.open_configuration(&alpn, Some(&settings))?;
// ... use connections ...

reg.shutdown();          // queue shutdown on all connections
reg.wait_idle().await;   // every Stream/Connection handle is now closed
drop(config);            // now safe
drop(reg);               // RegistrationClose returns promptly
```

`wait_idle()` covers streams too, so a `Stream` held past its `Connection` keeps
it pending rather than letting the registration be dropped out from under a
pending `StreamClose` (§9).

Server:

```rust
listener.stop().await?;  // graceful; optional
drop(listener);          // ListenerClose + ConfigurationClose
reg.shutdown();
reg.wait_idle().await;
drop(reg);
```

The order `shutdown()` then `wait_idle()` matters: `wait_idle()` alone will hang
if nothing has asked the connections to go away.

## 9. Why streams are tracked

`Stream`, `ReadStream`, and `WriteStream` are `Arc<StreamInstance>` holding a
`msquic::Stream` handle and **no reference at all** to the connection
(`stream.rs:33`, `:419-421`; `Stream::open` borrows `&msquic::Connection`,
`stream.rs:36-37`). A `Stream` can therefore outlive the last `Connection` clone.

An earlier revision of this document called that a hazard and left it out of
scope. Both halves of that were wrong.

### 9.1 `StreamClose` after `ConnectionClose` is fine

It is an explicitly supported, upstream-tested ordering:

- `QuicStreamInitialize` takes `QUIC_CONN_REF_STREAM` ("A stream depends on the
  connection", `connection.h:252`) at `stream.c:167`, and `QuicStreamFree`
  releases it at `stream.c:244`. `MsQuicConnectionClose` only drops
  `QUIC_CONN_REF_HANDLE_OWNER`, so the `QUIC_CONNECTION` allocation survives
  until the last stream is freed — it is never a use-after-free.
- `QuicConnCloseHandle` shuts streams down but frees none of them
  (`connection.c:477` → `QuicStreamSetShutdown`, `stream_set.c:193`). The
  application still owns every stream handle and must close each one.
- No deadlock either. The silent shutdown drives each stream to
  `ClientCallbackHandler = NULL`, so `MsQuicStreamClose` takes the
  `AlreadyShutdownComplete` fire-and-forget path (`api.c:898-911`) and never
  reaches the `CxPlatEventWaitForever` at `api.c:934`.
- `seera-msquic/src/test/lib/OwnershipTest.cpp:258-269`
  (`QuicTestConnectionCloseBeforeStreamClose`) closes the connection and then
  calls `Stream.GetID()` / `Stream.SetPriority()` before the stream's destructor
  runs `StreamClose`.

### 9.2 The real constraint is `RegistrationClose`, and it is our problem

`QuicConnCloseHandle` calls `QuicConnUnregister` →
`QuicRegistrationRundownRelease` (`connection.c:494`, `:512`). **The connection
releases the registration rundown as soon as its handle is closed, even with
streams still open.** `RegistrationClose` therefore does not wait for them, and
proceeds to `QuicWorkerPoolUninitialize` → `CXPLAT_FREE(WorkerPool)`
(`worker.c:1047-1055`); the worker pool is per-registration
(`registration.c:199-200`).

But `MsQuicStreamClose` calls `QuicConnQueueOper`, which unconditionally
enqueues onto `Connection->Worker` with no check of `HandleClosed` or
`ShutdownComplete` (`connection.c:726-754`). So:

```rust
let stream = conn.open_outbound_stream(..).await?;
drop(conn);                       // ConnectionClose; rundown released
registration.wait_idle().await;   // would resolve if streams were untracked
drop(registration);               // RegistrationClose frees the worker pool
drop(stream);                     // StreamClose queues onto freed memory
```

That is a genuine use-after-free. It predates `wait_idle()` — the native rundown
never protected against it — but `wait_idle()` makes it *worse* by giving callers
a credible signal that dropping the registration is now safe.

`docs/api/RegistrationClose.md` says only that configurations and connections
must be closed first; it does not mention streams. So this ordering requirement
is real but undocumented upstream.

### 9.3 Resolution

Streams get a `RundownGuard` (§5.4). Because the guard is about "may not outlive
`RegistrationClose`" rather than "holds the native rundown", tracking an object
that holds no rundown reference is correct rather than contradictory — it is
precisely the gap the native rundown fails to cover.

Consequence: `wait_idle()` now stays pending until every stream is closed, and
resolving it means dropping the `Registration` is safe.

## 10. Memory ordering

- Increment: `Relaxed` — no data is published by taking a reservation.
- Decrement: `Release`, plus an `Acquire` fence on the observed `1 → 0` edge, so
  the `*Close` that just ran is visible to whoever observes zero.
- `WaitIdle::poll`: `Acquire` load.
- Lost-wakeup window: `poll` registers its waker under the `Mutex<Waiters>` and
  then **re-checks** `active`, so a guard that drained to zero between the
  fast-path load and taking the lock cannot be missed.
- `WaitIdle::drop` deregisters its slot, so a cancelled waiter does not leak an
  entry in the waker map.

## 11. Migration

Breaking. Bump `msquic-async` to 0.5.0.

Call sites to update in-tree:

- `msquic-async/src/tests.rs` — 24 occurrences of
  `msquic::Registration::new(...)`, plus the helper signatures at `:2068`,
  `:2101` (`new_server`) and `:2158` (`new_client_config`), which take
  `&msquic::Registration` and internally call `msquic::Configuration::open`.
- `msquic-async/examples/{client,server}.rs` (`:17`, `:25`).
- `h3-msquic-async/examples/{h3-client,h3-server}.rs` (`:20`/`:65`, `:36`/`:128`).
- `README.md` (`:28`, `:158`) and `msquic-async/README.md`.

`h3-msquic-async`'s library code needs no change: it wraps an already-built
`msquic_async::Connection` and never touches a registration.

Applications that today rely on drop order alone keep working if they update the
type name — the wrapper is strictly additive at runtime — but they should adopt
`shutdown()` + `wait_idle()` to get the guarantee.

## 12. Testing

Unit tests on `RundownState`/`RundownGuard`/`WaitIdle` (portable from the prior
art, no msquic required):

1. Idle from the start resolves immediately.
2. A reservation keeps the future pending until dropped.
3. Dropping a guard wakes a registered waiter.
4. Multiple concurrent waiters all complete.
5. A dropped waiter deregisters its slot.
6. Nested reservations require all releases.

Integration tests against real msquic, in `tests.rs`, all shipped:

7. `test_registration_wait_idle_when_empty` — a registration with nothing
   derived from it is idle immediately.
8. `test_registration_wait_idle_tracks_listener` — pending while a `Listener`
   is alive, *still pending after `stop()` completes* (the event is not a
   close), and drains once the listener is dropped.
9. `test_registration_wait_idle_tracks_unstarted_connection` — a `Connection`
   that was never started is tracked.
10. `test_registration_wait_idle_after_connection_closed` — the full
    connect/shutdown/drop loop drains within a timeout. A timeout here is
    exactly the `RegistrationClose` hang being prevented.
11. `test_registration_wait_idle_tracks_connection_clones` — every clone must
    go before the registration drains; this is the §5.1 case that
    `h3-msquic-async` hits.
12. `test_registration_wait_idle_tracks_unaccepted_connection` — a connection
    accepted by MsQuic but never popped from `new_connections` keeps
    `wait_idle()` pending until the listener is dropped.

13. `test_registration_wait_idle_tracks_stream_outliving_connection` — a locally
    opened stream held past its connection keeps `wait_idle()` pending (§9).
14. `test_registration_wait_idle_tracks_inbound_stream` — the same for a
    peer-initiated stream, which is built in the connection callback.

Tests 13 and 14 were verified to be genuine regressions: with the stream guard
removed, both fail on the `assert_busy` step with "wait_idle() resolved while a
tracked handle was still alive".

Not covered: a `set_configuration` failure in the inbound path. Under the final
placement (§5.3) that path returns before any reservation exists, so there is no
leak to test.

## 13. Implementation checklist

- [x] Add `msquic-async/src/registration.rs` with `RundownState`, `RundownGuard`,
      `Registration`, `WaitIdle` + unit tests 1–6.
- [x] Export `Registration`, `WaitIdle` from `lib.rs`.
- [x] Add `_guard` to `ConnectionInstance` (declared after `msquic_conn`);
      change `Connection::new` to take `&crate::Registration`; add the `guard`
      parameter to `Connection::from_raw`.
- [x] Add `_guard` to `Listener` (declared after `msquic_listener`) and
      `rundown: Arc<RundownState>` to `ListenerInnerShared`; change
      `Listener::new` to take `&crate::Registration`.
- [x] Reserve in `handle_event_new_connection`.
- [x] Add the diagnostic `Drop for Registration`.
- [x] Update tests, examples, and both READMEs.
- [x] Add integration tests 7–12.
- [x] Bump `msquic-async` to 0.5.0 (and `h3-msquic-async` to 0.4.0, which
      depends on the breaking version).
- [x] Add `_guard` to `StreamInstance` (declared after `msquic_stream`) and
      `rundown: Arc<RundownState>` to `ConnectionInner`; add the `guard`
      parameter to `Stream::open` / `Stream::from_raw`; expose
      `RundownGuard::state()`. Integration tests 13–14.

Verified with `cargo clippy --workspace --all-targets` (clean) and
`cargo test -p msquic-async` (36 tests + 1 doctest, all passing), plus clippy
runs against the `msquic-2-5` and `msquic-seera` backends.
