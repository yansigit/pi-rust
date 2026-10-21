# Native authentication contract (work in progress)

`PluginApi::register_provider_auth` is an optional trailing slot. It preserves
all existing field offsets and the unified ABI version. A package that needs
this contract must fail registration with a useful missing-capability diagnostic
when the slot is absent. The older OAuth slot remains supported independently.
Adding a field to Rust struct literals is a source change to coordinate with
the maintainer's release; this patch does not bump workspace versions.

The manifest contains only public method discovery information:

```json
{"version":1,"id":"example","displayName":"Example","apiKey":true,"oauth":true,"methods":[{"id":"browser","name":"Browser login","kind":"oauth"}]}
```

Method IDs must be unique and nonempty, with kind `api_key` or `oauth`. A method
requires the corresponding capability. Unknown manifest fields and versions are
rejected. Registrations are first-wins within the owning registry; they are copied
into its snapshot and invalidated with that snapshot.

The callback receives `{version:1,providerId,action,input}` and a separate sized
`AuthRequestControl`. Actions are `login`, `continue`, `refresh`, `resolveApiKey`,
`toAuth`, and `revoke`. The control and its opaque context are borrowed until the
callback returns. The callback runs on a blocking worker thread and must poll
cancellation; it must contain panics with the SDK's guard helper.

Responses use `{version:1,kind,payload}`. Kinds are `interaction`, `credential`,
`auth`, `success`, and `unavailable`. Login interaction payloads use the private presentation contract below. Refresh input is
`{credential:{access,refresh,expires,...}}`, and its credential response payload
uses the same fields. Expiry is Unix **milliseconds**. `resolveApiKey` input is
`{env,credential}`, where environment values are a request snapshot. Its result
is `auth` or `unavailable`. `toAuth` receives an OAuth credential and returns
`auth`. Auth payload fields are `apiKey`, `headers`, `baseUrl`, `env`, and `source`;
omitted maps default to empty. The compatibility resolver assigns OAuth's source
label itself, as Pi 1.3.0 does.

The callback transfers its output to its registered deallocator exactly once,
including failure responses. Nonzero statuses, invalid JSON and unknown response
versions become redacted errors. Requests, continuations, credentials and resolved
authentication never pass through ordinary event dispatch. The bridge retains the
library and callback context through callback return and output reclamation even
when the caller cancels or the originating registry becomes stale.

`NativeProviderAuth::from_session` exposes the bridge to the host resolver.
`LegacyOAuthAuth::from_session` explicitly adapts an older OAuth registration,
converting milliseconds to the existing `expires_at` seconds at invocation and
back on return. Existing request-resolution paths do not opt into it automatically.
Legacy callbacks have no cancellation control; their library stays pinned until
they return even when the compatibility resolver stops waiting at its deadline.

`AuthRuntime` owns background refresh work and admitted native credential-file transactions. A host must await its `shutdown`
before stopping the async runtime. Refresh starts under the store lock, rechecks
the authoritative credential, has an independent 15-second deadline, and persists
rotation even if the request stops waiting. File-store mutation synchronizes the
temporary file, renames it, and synchronizes the parent directory on Unix. Windows
durability has not been qualified.

Native compatibility-store reads resolve API-key references with the credential environment, including escaped literals and cached commands. Locked mutations receive the raw stored value; request resolution does not rewrite it.

Native auth is applied by the CLI execution binding for every physical send.
`AuthRuntime::resolve_replacement` resolves an account with its own store and no
ambient environment. An unavailable account fails closed. The binding removes
bound credentials and credential headers, restores the trusted canonical endpoint
unless provider auth supplied another endpoint, and applies the account's resolved
environment. A caller-edited public model cannot change this destination. The SDK
preserves `authMode: replace`; protocol providers then refuse default keys and
ambient credential fallback. `from_private_request` is only for decoding that
already-resolved private boundary; native delegation must resolve through the
registered provider rather than accepting caller-supplied request authentication.

`rpi auth methods --provider ID` lists methods. `login` supports `--method ID`, or
presents the method list. `NativeProviderAuth::login` takes an awaited presentation
callback, validates results and acknowledges only a committed credential. CLI
interactions support:

- `input`: `title`, optional `message`/`url`, `secret`, `allowEmpty`, `continuation`.
- `select`: `title`, unique nonempty `choices: [{id,label}]`, `continuation`.
- `wait`: optional `message`, `pollAfterMs` (1–60000), `continuation`.

Continuation returns `{value,continuation}` for input/select and `{continuation}`
for wait. The opaque continuation never enters logging or observer events. Login
credential responses are tagged credentials (`type: api_key` or `type: oauth`);
refresh responses retain the untagged OAuth payload described above. The CLI's
`check` remains local and reports `refreshRequired` for near-expiry OAuth rather
than performing network activity. `refresh` uses the shared refresh runtime;
`logout` serializes revoke/removal against that same store lock.

Session shutdown and reload drain authentication before plugin teardown. A rejected
replacement session leaves the old session's auth runtime available. Response
metadata uses an awaited private observer, including failed HTTP responses before
stream events. The SDK negotiates this using `responseMetadata: true`, transports
`{type: response,response:{status,headers}}` privately, and drains that bounded
port while stream creation is still pending. Header values are byte arrays to
preserve duplicate and non-UTF-8 values. These frames never become agent events.
Normal request observer events contain null keys/headers and omit environment.

Native runtime/delegation, TUI presentation and package-level differential coverage
remain required before claiming the full authentication ledger complete.

Hosts advertising `auth.publicErrors` accept a successful callback response with
`kind: "error"` and `payload: {message: "..."}`. This is an explicitly public,
provider-redacted diagnostic, preserved through login, refresh and request-auth
conversion. Malformed envelopes and nonzero callback statuses retain their generic
errors. Provider packages must require the capability before depending on it.
`AuthError` now carries this diagnostic and is `Clone` rather than `Copy`; this is
a Rust source change for maintainer release coordination, with no C ABI change or
workspace version bump. Legacy providers keep their existing fallback behavior.

Capability discovery is synchronous and requires an active owning plugin. It is
safe during registration even when the loader uses a current-thread runtime;
it does not queue work onto that blocked runtime.

CLI authentication uses the same prepared extension bridge as inference, so
registration can inspect capabilities before a harness exists. Secret input uses
the terminal's non-echoing reader when attached to a terminal; piped input is read
without echoing it. Login and logout still await the shared credential store.
The loaded-library checks are recorded in [CLI evidence](host-auth-cli-results.json).
