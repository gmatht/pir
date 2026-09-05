# rustxwidgets

A thin, cross-platform GUI abstraction layer for Rust. Write your UI once — run it on **GTK3/4** (Linux), **native Windows** (via NWG), **WASM** (browser), **Android**, **Pancurses** (terminal), and **Zork** (interactive text adventure-style). Single source tree, single binary per target, no static linking of framework code.

GTK support uses `dlopen` so the binary (≈600 KB) runs on any Linux machine with GTK3 **or** GTK4 installed — no compile-time binding to a specific version.

Currently a prototype. Code AND API may change on whim.

## Backends

| Backend       | Platform               | Feature flag    | Screenshot |
|---------------|------------------------|-----------------|------------|
| **GTK3**      | Linux (dlopen)         | `gtk`           | ![GTK3](img/out/GTK3.png) |
| **GTK4**      | Linux (dlopen)         | `gtk`           | ![GTK4](img/out/GTK4.png) |
| **Windows**   | Windows (native)       | _(default)_     | ![Win](img/out/Win.png) |
| **Pancurses** | Linux/macOS (terminal) | `pancurses`     | ![Pancurses](img/out/Pancurses.png) |
| **WASM**      | Browser (web-sys)      | _(wasm32 arch)_ | ![WASM](img/out/WASM.png) |
| **Zork**      | Any (readline-style)   | `zork`          | ![Zork](img/out/Zork.png) |

## Backend auto-selection priority

1. **GTK** (Linux, feature `gtk`)
2. **NWG** (Windows)
3. **WASM** (wasm32 target)
4. **Android** (android target)
5. **Pancurses** (feature `pancurses`)
6. **Zork** (feature `zork`, fallback for all remaining platforms)

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
rustxwidgets = { path = "path/to/rustxwidgets" }
```

Run with a specific backend:

```bash
# GTK (Linux)
cargo run --features gtk

# Pancurses (terminal)
cargo run --features pancurses -- --pancurses

# Zork (text adventure fallback)
cargo run --features zork
```

## Widgets

- `Window`, `Button`, `Label`, `Entry`, `TextView`
- `Box` (horizontal/vertical), `Grid`
- `Menu`, `MenuBar`, `SimpleAction`
- `Dialog`, `DropDown`, `CheckButton`, `RadioButton`
- `Canvas` (2D drawing surface), `Overlay`, `ScrolledWindow`
- **Spreadsheet** — editable table with cell selection, keyboard navigation, column resize, styling (alignment, bold, highlight, text color)

## Building for WASM

```bash
cargo build --target wasm32-unknown-unknown
```

## License

MIT
