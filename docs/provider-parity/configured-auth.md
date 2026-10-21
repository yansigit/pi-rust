# Native configured authentication

Native authentication composes `models.json` API keys and provider/model headers
at each physical ambient send. Stored credentials and explicit request keys own
the provider; configured keys are consulted only when the credential slot is empty.
Values retain Pi 1.3's literal, template, escape and uncached-command behavior.
The private capability list advertises `auth.configuredValues`; packages that
depend on this behavior must require it during registration.
OAuth provider headers use credential environment; API-key headers also receive
the provider resolution's environment. Model headers and explicit request options
retain their respective precedence. No account switch mutates process environment.

Public catalog inspection and configured-key readiness checks never execute
configuration commands. Header values stay in private execution bindings. Those
bindings also keep the configured header names, so replacement attempts can remove
carried-over default-account headers even though public descriptors contain none.
Replacement credentials continue through the registered provider's authentication
implementation, bypassing ambient configuration.

[Evidence](configured-auth-results.json) records 600 comparisons against the actual
Pi 1.3 provider composer, provider regressions, command/template checks, an actual
loaded-library test, and the Rust 1.78 warning-denied workspace check. Regenerate
the reference with `scripts/provider-config-auth-reference.mjs`, setting
`RPI_INTEGRATED_REFERENCE` and `RPI_REFERENCE_OUTPUT` to the pinned installation
and `configured-auth-reference.json` respectively.

This does not complete host or package parity. Full `modelOverrides`, configuration
insertion order, provider-defined readiness callbacks, Windows shell execution,
shell timeout/cancellation qualification and all three packages' differential
acceptance remain open. Existing legacy-provider resolution is unchanged.
