# Cursor SDK distribution review

Reviewed 2026-10-21 for the pinned `@cursor/sdk@1.0.37` package. Its
`LICENSE.md` reserves rights to Anysphere and points to the
[Cursor Terms of Service](https://cursor.com/terms-of-service), updated September
3, 2026. Sections 1.5 and 5.1 restrict reproduction/modification and reserve
ungranted rights. This review does not establish an independent redistribution
license for the SDK or its platform binaries.

The [official SDK documentation](https://cursor.com/docs/sdk/typescript) describes
installing `@cursor/sdk` through npm and building applications using it. It also
documents bundled builds and separate platform binaries. Those current packaging
instructions are not substituted for the behavior of version 1.0.37.

Distribution decision: retain the MIT-licensed pinned Cursor adapter and its
license notices in the integration fork; keep the proprietary SDK and platform
packages external to rpi release archives. The installed worker resolves an exact
1.0.37 dependency installed from npm using its lockfile. Packaging acceptance must
inspect the final archive to verify that it contains no SDK implementation or
platform binaries. Offline execution requires that dependency installation to
have completed. No upstream SDK source modifications are planned.
