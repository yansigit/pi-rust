# Awaited native dialogs

Native clients call the existing versioned `native_request` slot with
`port: "dialogs"`, `action: "show"` and a declarative `ui` object. The request
waits for a validated answer. Missing interactive UI, malformed controls and
exhausted capacity return errors. User cancellation, UI detach and owner shutdown
return `null`; invocation cancellation can instead return a cancellation error.
The SDK client must capture its invocation context before moving to a worker.

```json
{
  "port": "dialogs",
  "action": "show",
  "ui": {
    "kind": "select",
    "title": "Select account",
    "searchable": true,
    "query": "work",
    "selected": "account-2",
    "choices": [
      { "value": "account-1", "label": "Work", "description": "First account" },
      { "value": "account-2", "label": "Work", "description": "Second account" }
    ]
  }
}
```

The response to the example is `{"type":"select","values":["account-2"],
"query":"work"}`. Labels may repeat; values must be unique. Local and remote
terminals use the same input, search, selection and settings components. Search
matches display labels. `selected` restores the highlighted stable ID after a
submenu, and `query` restores its search text.
Native controls retain Pi 1.3.0's fractional search scores, UTF-16 match positions,
alphanumeric fallback and JavaScript whitespace rules. Existing host selectors
retain their current ranking. `scripts/provider-dialog-reference.mjs` executes
the pinned host to regenerate the 276-case ranking fixture.

Supported controls:

| Kind | Fields and replies |
| --- | --- |
| `input` | `initialValue`, `placeholder`, `masked`, `trim`, optional `integerMin`; reply `{"type":"input","value":"text"}` |
| `select` | `choices`, `searchable`, `query`, `selected`, `multiple`, `freeform`; reply `{"type":"select","values":["id"],"query":"..."}` |
| `confirm` | `title`, optional `description`, optional `selected: "true"` or `"false"`; reply `{"type":"confirm","confirmed":true}` |
| `settings` | `settings`, `searchable`, `query`, `selected`; reply `{"type":"change","id":"...","value":"...","query":"..."}` or `{"type":"open","id":"...","query":"..."}` |

A settings row has `id`, `label`, `value`, optional `description`, and either
`values` to cycle or `submenu: true`. Each action completes its awaited request.
The owning extension persists the change or opens its submenu, then reopens the
parent using the returned ID and query. This keeps persistence and nested menu
policy in the extension. Selecting freeform returns `type: "freeform"` with the
already selected IDs; the extension asks for its text in the next input dialog.

Integer validation follows the pinned multiprovider `IntegerInputSubmenu`:
trimmed ASCII decimal digits, JavaScript's safe integer range, inclusive minimum,
and normalized decimal output. Invalid input keeps the dialog open and displays
the original validation message. The next key or paste clears that message.
Settings Space behavior also follows that control: it activates the row with an
empty query, and adds a search space with a nonempty query.

Host-side validation also applies to remote replies, including selected IDs,
declared settings values, submenu actions and integer bounds. Unknown schema
fields, duplicate IDs and oversized payloads are rejected. Controls and replies
are limited to 4 MiB; controls have at most 8,192 rows, with at most 64 pending
dialogs per owner and 512 per mailbox.

The host generates the request ID and binds its answer to the actual plugin
generation. Legacy polling or another owner cannot consume or replace a private
reply. Answers do not enter ordinary plugin events or conversation history.
Cancelled/dropped requests remove their queued and active records; the local
terminal also dismisses a visible request cancelled by a worker. Shutdown cancels
only the departing owner's requests and rejects new requests from that owner.
Existing polling dialogs and C ABI layouts remain unchanged.

This supplies H09 host controls. Complete account/login/virtual-provider command
traces and persisted outcomes remain part of the three package acceptance suites.

## TUI provider-auth callers (H01)

The interactive TUI now drives registered native provider authentication through
the same awaited dialog port as every other terminal caller. `/login` (the
provider menu, `/login <provider>` without a key, and the provider-scoped
arguments) checks `NativeProviderAuth::from_session` first; a native
registration runs `NativeProviderAuth::login`/`ProviderAuthentication::request`
on the session runtime and presents each `AuthInteraction` as a declarative
control. Host API-key providers (the compatibility `ApiKeyAuthentication`
fallback) use the same driver, so the `/login` menu no longer opens an inline
masked input for either case:

- `input` becomes `kind: "input"` with `masked` from `secret`, the optional
  message and URL as its description, and a re-prompt when a required value is
  empty (the provider's original `allowEmpty` policy is preserved).
- `select` becomes a searchable `kind: "select"` whose stable `value` is the
  provider's choice ID, so re-opened submenus restore the highlighted method.
- `wait` reports its message and awaits the provider's `pollAfterMs` with the
  flow's cancellation token.
- Multiple methods present a method `select` before login.

The prompt is a `ui_dialog_request` on the session's `UiDialogMailbox`; the TUI
key loop renders it with `NativeDialogView`, and the RPC terminal renders the
same request. Both answer with the same validated reply envelope, so scripted
choices, messages and persisted outcomes are identical across terminal modes.
Host API-key registrations use the same presentation callback; legacy OAuth
registrations keep their existing extension request form. Credentials are
committed only through the shared locked `CompatibilityCredentialStore`, and a
successful commit asks the main loop to reload the provider catalog.

Cancellation releases the pending prompt in every case: Esc on the dialog
cancels the mailbox request, the flow id (and a reload that invalidates the
owning registry) cancels the awaiting token, and TUI shutdown detaches the
mailbox. The prompt's drop guard also cancels a request whose login task is
aborted, so no orphan control survives a session replacement. `Esc` during a
provider round-trip without a visible dialog still ends the flow through the
existing provider-auth slot, now shared with native login.

