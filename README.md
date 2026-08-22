# zeron

Desktop app — GPUI shell, composer, transcript, settings. Depends on backend crates from [comet](https://github.com/balnoumair/comet) and the [onyx-ui](https://github.com/balnoumair/onyx-ui) design system.

## Layout

```
apps/zeron/          `zeron` binary (headed UI + headless engine CLI)
crates/zeron-ui/     Product UI (shell, composer, transcript, …)
```

## Dependencies

| Crate | Source |
| --- | --- |
| `zeron-proto`, `zeron-doc`, `zeron-engine`, `zeron-harness`, `zeron-rpc` | [comet](https://github.com/balnoumair/comet) |
| `onyx-ui` | [onyx-ui](https://github.com/balnoumair/onyx-ui) |

Clone all three repos as siblings for local development:

```
Projects/
  comet/
  onyx-ui/
  zeron/      ← this repo
```

The workspace `Cargo.toml` uses path dependencies to `../comet` and `../onyx-ui`. For CI or standalone clones, switch those to git dependencies pinned to a revision.

Take gpui through `onyx_ui::gpui` — never depend on gpui directly.

## Build

```bash
cargo build
cargo run -p zeron          # headed UI
cargo run -p zeron -- headless
```

## License

MIT
