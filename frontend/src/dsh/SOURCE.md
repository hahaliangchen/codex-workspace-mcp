# DeepSeek Harness browser sources

These files were copied from DeepSeek Harness commit `46a7f68` under the MIT
license in `LICENSE`:

- `cordis/` ← `vendor/cordis/src/`: complete Cordis TypeScript core. Its
  upstream MIT license is preserved as `cordis/LICENSE`.
- `ui-slots/` ← `packages/client/ui-slots/src/`: slot contracts, registration
  ledger, dispatch rules, and renderer interface.
- `ui-renderer/` ← `packages/client/ui-renderer/src/client/`: Cordis slot service,
  React outlets, bindings, error boundaries, and application mount.
- Other directories retain the source mapping documented in the root README.

The application-specific Cordis composition and Rust API adaptation live in
`frontend/src/cordis/`. The copied source is imported through Vite and
TypeScript aliases so it remains readable and editable here. Cordis itself is
compiled from the local source by Vite; TypeScript resolves its public types
from the pinned `@deepseek-ai/cordis` package because its upstream source uses
a different strictness profile than this frontend project.
