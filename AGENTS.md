# Working on edualc as an agent

- **While you work:** `just feedback <package> [test filter]`, with the package you changed
  (`edualc-core`, `edualc-host` or `edualc`). It formats, then runs ast-grep's rules, clippy and
  that package's tests, one line per finding, and stops at the first stage that fails. A change to
  `edualc-core` needs the loops of `edualc-host` and `edualc` too. `just scan` runs the rules
  alone, without a build.
- **Before you hand in:** `just lint` and `just test` must pass; `just lint-wasm` too after a
  change to `edualc-core` or `edualc`.
- **The rules** are in `lint/rules/`, each with its reason. Fix the code; never add `#[allow]`,
  and don't change a rule or its list of exceptions to make a check pass.
- **Generated files:** after changing `ui/types.slint` or `ui/state.slint`, run `just present`;
  don't edit `crates/core/src/present/model.rs` or `crates/slint/src/ui/model.rs` by hand.
- **JSON** that edualc writes or reads is a typed value with serde, the wire's types in
  `crates/core/src/external/`; `json!` is for tests only.
- Docs, interface text and comments are plain English to "you". Comments say why, not what.
