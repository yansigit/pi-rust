# Owned native commands and notifications

`PluginApi.register_native_command` is an optional trailing slot. It accepts the
existing sized `AuthRequestFn` control and a manifest
`{version:1,name,description}`, plus the plugin's deallocator and context. Existing
C slots and signatures retain their offsets. Missing slots remain `None` when a
new SDK reads an older, shorter vtable. Packages must require `commands.owned` and
`ui.notifications` before using these contracts.

The host invokes `{version:1,command,args}` on a blocking worker. The callback
returns `{version:1,result}` or `{version:1,error}`. Commands may await private
native dialogs while the interactive/RPC input loop continues reading. Native
commands own their callback context; legacy command handlers retain the original
host-handle contract. A command's callback, library and capacity permit remain
alive until foreign code returns, even after cancellation. Registration, command
invocation and tool invocation remain separate scopes. Finishing a command does
not retire its owning plugin. Notifications and dialogs never start an agent turn.

Admission rejects expired registrations, foreign sessions, cancelled owners,
arguments over 1 MiB, and more than 16 outstanding invocations of one registered
command. Cancellation does not free capacity for a callback that is still running.
Owner shutdown cancels admitted commands. An obsolete interactive invocation
cannot render its completion into a replacement session. These changes add an
optional `native` field to the host's Rust `RegisteredCommand` and change the
internal interactive result type and RPC entry point session argument; source
release coordination is required, with
no contributor version bump.

The interactive slash-command registry includes the native command's name and
description. RPC clients use `get_commands` and
`{type:"extension_command",id,name,args?}`. The RPC response follows command
completion and its queued UI notifications. `ui_dialog_response` and other RPC
requests remain available while a command awaits input. RPC limits itself to 64
outstanding command response tasks. EOF/shutdown cancels commands and drains their
callbacks. This is a native integration contract; it does not load TypeScript
extensions or add arbitrary TypeScript command handlers to RPC.

Native clients publish `{port:"ui",action:"notify",text,kind?}`; kind defaults to
`info` and also accepts `warning` or `error`. The host captures the session owner,
so JSON cannot choose a destination. Local terminals render these notices;
attached RPC terminals receive `{type:"ui_notification",text,kind}`. Notifications
and dialog requests retain their publication order. Notices have no answer and do
not append conversation/session entries or enter plugin observer events.

The mailbox permits 128 queued notices per owner, 512 in total, 4 MiB of queued
text and 1 MiB per notice. Malformed kinds and exhausted capacity are errors.
Unattached hosts accept notices without retaining them. Dialog cancellation keeps
queued notices; owner retirement removes that owner's queued notices and refuses
late publications. Detaching the terminal clears its pending UI work. The provider
is responsible for redacting its own display text before publication.

[Verification results](host-native-commands-results.json) cover Linux native
loading, lifecycle, RPC, SDK layout and regression checks with Rust 1.78 and
warnings denied. The workspace/all-targets compile check passed. Optional Clippy
reports existing lint failures in unchanged code. Package-level command acceptance
and other platform execution remain separate parity requirements.
